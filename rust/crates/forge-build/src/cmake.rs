//! CMake project resolution: root discovery, `CMakeLists.txt` auto-generation,
//! and File-API executable discovery. Ports build-runner.ts:243-416.

use std::path::{Path, PathBuf};

/// Build directory name, kept separate from CLion's `cmake-build-debug`
/// (build-runner.ts:11).
pub const BUILD_DIR_NAME: &str = "cmake-build-forge";

/// Walk up from `start_dir` looking for a `CMakeLists.txt` project root. Stops
/// at `$HOME` (or after 8 levels, or at `/`) so a stray ancestor project isn't
/// picked up (build-runner.ts `findCMakeRoot`).
pub fn find_cmake_root(start_dir: &Path, home: &Path) -> Option<PathBuf> {
    let mut dir = start_dir.to_path_buf();
    for _ in 0..8 {
        if dir.join("CMakeLists.txt").exists() {
            return Some(dir);
        }
        if dir == home || dir == Path::new("/") {
            break;
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => break,
        }
    }
    None
}

fn has_ext(name: &str, exts: &[&str]) -> bool {
    name.rsplit_once('.')
        .map(|(_, e)| exts.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Compute the `CMakeLists.txt` content for a directory with none, modeled on
/// ClionProjects/metalllm (build-runner.ts `generateCMakeLists`). Returns
/// `(target_name, contents)`, or `None` when there is no buildable source and
/// no Metal shader (the TS `return false`). Pure: does no IO writing — the
/// caller persists it — but it does read `cwd` to select sources / detect Metal.
pub fn build_cmake_lists(cwd: &Path, active_file: &Path) -> Option<(String, String)> {
    let ext = active_file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let siblings: Vec<String> = std::fs::read_dir(cwd)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();

    // Pick the executable's source (build-runner.ts:266-274).
    let source_file: Option<String> = if ext == "metal" {
        ["main.cpp", "main.mm", "main.cc", "main.m"]
            .iter()
            .find(|f| siblings.iter().any(|s| s == **f))
            .map(|s| s.to_string())
            .or_else(|| {
                let mut cands: Vec<String> = siblings
                    .iter()
                    .filter(|f| has_ext(f, &["cpp", "cc", "mm", "m"]))
                    .cloned()
                    .collect();
                cands.sort();
                cands.into_iter().next()
            })
    } else {
        active_file
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
    };

    let is_objc = source_file
        .as_deref()
        .map(|s| has_ext(s, &["mm", "m"]))
        .unwrap_or(false);
    let has_metal = siblings.iter().any(|f| f.ends_with(".metal"));

    let raw_name = source_file
        .as_deref()
        .map(|s| {
            let base = Path::new(s).file_stem().and_then(|x| x.to_str()).unwrap_or(s);
            base.to_string()
        })
        .unwrap_or_else(|| {
            cwd.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("app")
                .to_string()
        });
    let sanitized: String = raw_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let target = if sanitized.is_empty() {
        "app".to_string()
    } else {
        sanitized
    };

    if source_file.is_none() && !has_metal {
        return None;
    }

    let mut lines: Vec<String> = vec![
        "cmake_minimum_required(VERSION 3.20)".into(),
        format!(
            "project({} {})",
            target,
            if is_objc { "OBJCXX CXX" } else { "CXX" }
        ),
        String::new(),
        "set(CMAKE_CXX_STANDARD 17)".into(),
        "set(CMAKE_CXX_STANDARD_REQUIRED ON)".into(),
        String::new(),
    ];

    if let Some(src) = &source_file {
        lines.extend([
            format!("add_executable({} {})", target, src),
            String::new(),
            "# Forge IDE instrumentation headers (idetools.h), passed at configure time".into(),
            "if(DEFINED FORGE_INCLUDE_DIR)".into(),
            format!("    target_include_directories({} PRIVATE ${{FORGE_INCLUDE_DIR}})", target),
            "endif()".into(),
            String::new(),
        ]);
        if is_objc || has_metal {
            lines.extend([
                format!("target_link_libraries({} PRIVATE", target),
                "        \"-framework Metal\"".into(),
                "        \"-framework MetalPerformanceShaders\"".into(),
                "        \"-framework Foundation\"".into(),
                "        \"-framework QuartzCore\"".into(),
                ")".into(),
                String::new(),
            ]);
        }
    }

    if has_metal {
        lines.extend([
            "# ── Compile any sibling .metal shaders into default.metallib at build time ──".into(),
            "file(GLOB METAL_SHADERS \"${CMAKE_SOURCE_DIR}/*.metal\")".into(),
            "if(METAL_SHADERS)".into(),
            "    set(AIR_FILES \"\")".into(),
            "    foreach(SHADER ${METAL_SHADERS})".into(),
            "        get_filename_component(SHADER_NAME ${SHADER} NAME_WE)".into(),
            "        set(AIR_FILE \"${CMAKE_BINARY_DIR}/${SHADER_NAME}.air\")".into(),
            "        add_custom_command(".into(),
            "                OUTPUT ${AIR_FILE}".into(),
            "                COMMAND xcrun -sdk macosx metal -c ${SHADER} -o ${AIR_FILE}".into(),
            "                DEPENDS ${SHADER}".into(),
            "                COMMENT \"Compiling Metal shader ${SHADER_NAME}.metal\"".into(),
            "        )".into(),
            "        list(APPEND AIR_FILES ${AIR_FILE})".into(),
            "    endforeach()".into(),
            String::new(),
            "    set(METALLIB \"${CMAKE_BINARY_DIR}/default.metallib\")".into(),
            "    add_custom_command(".into(),
            "            OUTPUT ${METALLIB}".into(),
            "            COMMAND xcrun -sdk macosx metallib ${AIR_FILES} -o ${METALLIB}".into(),
            "            DEPENDS ${AIR_FILES}".into(),
            "            COMMENT \"Linking default.metallib\"".into(),
            "    )".into(),
            format!(
                "    add_custom_target(metal_shaders {}DEPENDS ${{METALLIB}})",
                if source_file.is_some() { "" } else { "ALL " }
            ),
        ]);
        if source_file.is_some() {
            lines.push(format!("    add_dependencies({} metal_shaders)", target));
        }
        lines.push("endif()".into());
        lines.push(String::new());
    }

    Some((target, lines.join("\n")))
}

/// Canonicalize when possible, else normalize lexically, so path comparison is
/// robust to `.`/symlink differences the way `path.resolve` string-equality was.
fn norm(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn resolve_against(base: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Locate the built executable via CMake's File API (codemodel-v2 reply),
/// preferring the executable target whose sources include `active_file`
/// (build-runner.ts `findBuiltExecutable`). Returns `None` on any parse failure.
pub fn find_built_executable(
    build_dir: &Path,
    source_root: &Path,
    active_file: &Path,
) -> Option<PathBuf> {
    let reply_dir = build_dir.join(".cmake/api/v1/reply");
    let mut index_files: Vec<PathBuf> = std::fs::read_dir(&reply_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("index-") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();
    index_files.sort();
    let index_file = index_files.pop()?;

    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&index_file).ok()?).ok()?;
    let reply = index.get("reply")?;
    let cm_json = reply
        .get("codemodel-v2")
        .and_then(|r| r.get("jsonFile"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            reply.as_object()?.values().find_map(|r| {
                if r.get("kind").and_then(|k| k.as_str()) == Some("codemodel") {
                    r.get("jsonFile").and_then(|v| v.as_str())
                } else {
                    None
                }
            })
        })?;

    let codemodel: serde_json::Value =
        serde_json::from_slice(&std::fs::read(reply_dir.join(cm_json)).ok()?).ok()?;
    let config = codemodel.get("configurations")?.get(0)?;

    struct Exe {
        artifact: PathBuf,
        sources: Vec<PathBuf>,
    }
    let mut executables: Vec<Exe> = Vec::new();
    for t in config.get("targets")?.as_array()?.iter() {
        let tjf = t.get("jsonFile").and_then(|v| v.as_str())?;
        let tj: serde_json::Value = match std::fs::read(reply_dir.join(tjf))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
        {
            Some(v) => v,
            None => continue,
        };
        if tj.get("type").and_then(|v| v.as_str()) != Some("EXECUTABLE") {
            continue;
        }
        let Some(artifact_path) = tj
            .get("artifacts")
            .and_then(|a| a.get(0))
            .and_then(|a| a.get("path"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        let sources = tj
            .get("sources")
            .and_then(|s| s.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.get("path").and_then(|v| v.as_str()))
                    .map(|p| norm(&resolve_against(source_root, p)))
                    .collect()
            })
            .unwrap_or_default();
        executables.push(Exe {
            artifact: resolve_against(build_dir, artifact_path),
            sources,
        });
    }
    if executables.is_empty() {
        return None;
    }

    let active = norm(active_file);
    let owning = executables.iter().find(|e| e.sources.contains(&active));
    Some(owning.unwrap_or(&executables[0]).artifact.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_discovery_finds_nearest_and_stops_at_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = home.join("proj");
        let sub = proj.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(proj.join("CMakeLists.txt"), "").unwrap();
        assert_eq!(
            find_cmake_root(&sub, &home).map(|p| std::fs::canonicalize(p).unwrap()),
            Some(std::fs::canonicalize(&proj).unwrap())
        );
        // A dir under home with no CMakeLists (and none up to home) -> None.
        let lonely = home.join("lonely");
        std::fs::create_dir_all(&lonely).unwrap();
        assert_eq!(find_cmake_root(&lonely, &home), None);
    }

    #[test]
    fn generate_for_plain_cpp() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("main.cpp");
        std::fs::write(&src, "int main(){}").unwrap();
        let (target, content) = build_cmake_lists(dir.path(), &src).unwrap();
        assert_eq!(target, "main");
        assert!(content.contains("project(main CXX)"));
        assert!(content.contains("add_executable(main main.cpp)"));
        assert!(content.contains("target_include_directories(main PRIVATE ${FORGE_INCLUDE_DIR})"));
        assert!(!content.contains("QuartzCore"));
    }

    #[test]
    fn generate_for_objc_links_frameworks() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("app.mm");
        std::fs::write(&src, "int main(){}").unwrap();
        let (target, content) = build_cmake_lists(dir.path(), &src).unwrap();
        assert_eq!(target, "app");
        assert!(content.contains("project(app OBJCXX CXX)"));
        assert!(content.contains("-framework Metal"));
        assert!(content.contains("-framework QuartzCore"));
    }

    #[test]
    fn generate_for_metal_active_file_picks_host_and_shader_chain() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.cpp"), "int main(){}").unwrap();
        std::fs::write(dir.path().join("kernel.metal"), "// shader").unwrap();
        let shader = dir.path().join("kernel.metal");
        let (target, content) = build_cmake_lists(dir.path(), &shader).unwrap();
        assert_eq!(target, "main"); // host program selected, not the .metal
        assert!(content.contains("add_executable(main main.cpp)"));
        assert!(content.contains("default.metallib"));
        assert!(content.contains("xcrun -sdk macosx metal -c"));
        assert!(content.contains("add_dependencies(main metal_shaders)"));
    }

    #[test]
    fn no_source_no_metal_returns_none() {
        // A `.metal` active file in an empty dir: no host source is found and no
        // sibling `.metal` exists on disk, so there is nothing to generate.
        let dir = tempfile::tempdir().unwrap();
        let ghost = dir.path().join("ghost.metal");
        assert_eq!(build_cmake_lists(dir.path(), &ghost), None);
    }
}
