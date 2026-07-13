//! `compile()` — CMake configure (with caching) + build, then File-API
//! executable discovery (build-runner.ts:418-511).

use std::path::Path;
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::cmake::{build_cmake_lists, find_built_executable, find_cmake_root, BUILD_DIR_NAME};
use crate::parse::parse_compiler_errors;
use crate::types::*;
use crate::BuildEngine;

/// Text sink for streamed build output (cmake logs + engine notices). A plain
/// string channel: every item is already newline-terminated.
pub type OutputSink = UnboundedSender<String>;

fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

/// Run a `cmake` invocation, streaming merged stdout/stderr to `out` and
/// collecting it for error parsing (build-runner.ts `runCMake`).
async fn run_cmake(args: &[String], cwd: &Path, out: &OutputSink) -> (i32, String) {
    let mut cmd = tokio::process::Command::new("cmake");
    cmd.args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (-1, format!("Failed to start cmake: {e}")),
    };
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let mut so = BufReader::new(stdout).lines();
    let mut se = BufReader::new(stderr).lines();
    let mut output = String::new();
    let (mut so_done, mut se_done) = (false, false);

    loop {
        tokio::select! {
            l = so.next_line(), if !so_done => match l {
                Ok(Some(x)) => { output.push_str(&x); output.push('\n'); let _ = out.send(format!("{x}\n")); }
                _ => so_done = true,
            },
            l = se.next_line(), if !se_done => match l {
                Ok(Some(x)) => { output.push_str(&x); output.push('\n'); let _ = out.send(format!("{x}\n")); }
                _ => se_done = true,
            },
        }
        if so_done && se_done {
            break;
        }
    }

    let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
    (code, output)
}

/// Port of build-runner.ts `compile()`.
pub async fn compile(engine: &BuildEngine, req: &CompileRequest, out: &OutputSink) -> BuildResult {
    let cwd = req
        .file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let start = Instant::now();
    let forge_include_dir = &engine.config.forge_include_dir;

    // ── Resolve (or bootstrap) the CMake project ──
    let root = match find_cmake_root(&cwd, &home_dir()) {
        Some(r) => r,
        None => match build_cmake_lists(&cwd, &req.file) {
            Some((target, content)) => {
                if let Err(e) = std::fs::write(cwd.join("CMakeLists.txt"), content) {
                    let _ = out.send(format!(
                        "\x1b[31m[cmake]\x1b[0m Failed to write CMakeLists.txt: {e}\n"
                    ));
                    return BuildResult {
                        success: false,
                        executable: None,
                        errors: vec![BuildError {
                            file: req.file.clone(),
                            line: 0,
                            column: 0,
                            message: format!("Failed to write CMakeLists.txt: {e}"),
                            severity: Severity::Error,
                        }],
                        duration: start.elapsed(),
                    };
                }
                let _ = out.send(format!(
                    "\x1b[36m[forge]\x1b[0m No CMakeLists.txt found — generated one for target '{target}'\n"
                ));
                cwd.clone()
            }
            None => {
                return BuildResult {
                    success: false,
                    executable: None,
                    errors: vec![BuildError {
                        file: req.file.clone(),
                        line: 0,
                        column: 0,
                        message: "No CMakeLists.txt found and no buildable source to generate one from".into(),
                        severity: Severity::Error,
                    }],
                    duration: start.elapsed(),
                };
            }
        },
    };
    let build_dir = root.join(BUILD_DIR_NAME);

    // ── Flags → CMake cache variables ──
    let mut cxx_flags: Vec<String> = vec!["-g".to_string()];
    cxx_flags.extend(req.flags.iter().cloned());
    let mut ld_flags: Vec<String> = Vec::new();
    if req.sanitize {
        cxx_flags.push("-fsanitize=address".into());
        cxx_flags.push("-fno-omit-frame-pointer".into());
        ld_flags.push("-fsanitize=address".into());
    }
    if req.instrument {
        cxx_flags.push("-fprofile-arcs".into());
        cxx_flags.push("-ftest-coverage".into());
        ld_flags.push("--coverage".into());
    }

    let configure_args: Vec<String> = vec![
        "-S".into(),
        root.to_string_lossy().into_owned(),
        "-B".into(),
        build_dir.to_string_lossy().into_owned(),
        "-DCMAKE_BUILD_TYPE=Debug".into(),
        "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".into(),
        format!("-DFORGE_INCLUDE_DIR={}", forge_include_dir.display()),
        format!("-DCMAKE_CXX_FLAGS={}", cxx_flags.join(" ")),
        format!("-DCMAKE_OBJCXX_FLAGS={}", cxx_flags.join(" ")),
        format!("-DCMAKE_EXE_LINKER_FLAGS={}", ld_flags.join(" ")),
    ];

    // ── Configure (skipped when args unchanged and cache exists) ──
    let configure_key = configure_args.join("\u{1f}");
    let needs_configure = engine
        .configured_builds
        .lock()
        .unwrap()
        .get(&build_dir)
        .map(|k| k != &configure_key)
        .unwrap_or(true)
        || !build_dir.join("CMakeCache.txt").exists();

    if needs_configure {
        // File API query so the reply tells us where executable artifacts land.
        let query_dir = build_dir.join(".cmake/api/v1/query");
        let _ = std::fs::create_dir_all(&query_dir);
        let _ = std::fs::write(query_dir.join("codemodel-v2"), b"");

        let _ = out.send(format!(
            "\x1b[36m[cmake]\x1b[0m Configuring {}\n",
            root.display()
        ));
        let (code, output) = run_cmake(&configure_args, &root, out).await;
        if code != 0 {
            engine.configured_builds.lock().unwrap().remove(&build_dir);
            return BuildResult {
                success: false,
                executable: None,
                errors: parse_compiler_errors(&output, &root),
                duration: start.elapsed(),
            };
        }
        engine
            .configured_builds
            .lock()
            .unwrap()
            .insert(build_dir.clone(), configure_key);
    }

    // ── Build ──
    let build_args: Vec<String> = vec![
        "--build".into(),
        build_dir.to_string_lossy().into_owned(),
        "--parallel".into(),
    ];
    let (code, output) = run_cmake(&build_args, &root, out).await;
    let errors = parse_compiler_errors(&output, &root);
    if code != 0 {
        return BuildResult {
            success: false,
            executable: None,
            errors,
            duration: start.elapsed(),
        };
    }

    let executable = find_built_executable(&build_dir, &root, &req.file);
    BuildResult {
        success: true,
        executable,
        errors,
        duration: start.elapsed(),
    }
}
