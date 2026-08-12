//! End-to-end: generate a `CMakeLists.txt` for a tiny C++ program, compile it
//! with the real `cmake`/`clang`, run it, and assert its output + exit code.
//! Skips gracefully (passes) if `cmake` is not installed.

use std::path::PathBuf;

use jade_build::{BuildEngine, CompileRequest, EngineConfig, RunConfig, RunEvent};
use tokio::sync::mpsc;

fn cmake_available() -> bool {
    std::process::Command::new("cmake")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The repo root, so the engine can find `include/`, the interposer, and probe.
fn repo_root() -> PathBuf {
    // crate dir = <root>/rust/crates/jade-build
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap()
}

#[tokio::test]
async fn compile_and_run_tiny_program() {
    if !cmake_available() {
        eprintln!("cmake not found — skipping integration test");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("hello.cpp");
    std::fs::write(
        &src,
        r#"#include <cstdio>
int main() {
    std::printf("jade-build-ok 42\n");
    return 7;
}
"#,
    )
    .unwrap();

    let engine = BuildEngine::new(EngineConfig::from_repo_root(&repo_root()));

    // ── compile ──
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let req = CompileRequest {
        file: src.clone(),
        ..Default::default()
    };
    let build = engine.compile(&req, &out_tx).await;
    drop(out_tx);
    let mut build_log = String::new();
    while let Some(l) = out_rx.recv().await {
        build_log.push_str(&l);
    }

    assert!(
        build.success,
        "compile failed: errors={:?}\nlog=\n{}",
        build.errors, build_log
    );
    // A CMakeLists.txt was auto-generated (none existed).
    assert!(tmp.path().join("CMakeLists.txt").exists());
    // The generation notice was streamed.
    assert!(
        build_log.contains("generated one for target 'hello'"),
        "missing generation notice; log=\n{build_log}"
    );
    let exe = build.executable.expect("executable path resolved via File API");
    assert!(exe.exists(), "built executable should exist at {exe:?}");

    // ── run ──
    let mut handle = engine.run(RunConfig {
        executable: exe,
        ..Default::default()
    });
    let mut stdout = String::new();
    while let Some(ev) = handle.events.recv().await {
        if let RunEvent::Output(s) = ev {
            stdout.push_str(&s);
        }
    }
    let result = handle.result.await.unwrap();

    assert!(
        stdout.contains("jade-build-ok 42"),
        "program stdout missing; got: {stdout:?}"
    );
    assert_eq!(result.exit_code, 7, "exit code should propagate");
}

#[tokio::test]
async fn compile_reports_errors_for_bad_source() {
    if !cmake_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("bad.cpp");
    std::fs::write(&src, "int main() { this is not valid c++ }\n").unwrap();

    let engine = BuildEngine::new(EngineConfig::from_repo_root(&repo_root()));
    let (out_tx, _out_rx) = mpsc::unbounded_channel::<String>();
    let build = engine
        .compile(
            &CompileRequest {
                file: src,
                ..Default::default()
            },
            &out_tx,
        )
        .await;

    assert!(!build.success, "expected compile failure");
    assert!(
        build.errors.iter().any(|e| matches!(
            e.severity,
            jade_build::Severity::Error
        )),
        "expected at least one error diagnostic, got {:?}",
        build.errors
    );
}
