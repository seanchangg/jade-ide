//! `atos` buffer-alias symbolication — the toolchain half of
//! `telemetry-server.ts` `resolveBufferAlias` + `extractVarName` (`:239-319`).
//!
//! [`AtosSymbolicator`] implements [`forge_telemetry::Symbolicator`]; the
//! telemetry server installs it via `set_symbolicator` and drives it when a
//! probe declares a buffer carrying allocation-site addresses. This half runs
//! `atos`, maps frames back to `file:line`, and recovers the variable name the
//! allocation was assigned to; the server owns the collision-resolution and
//! rename that turn those names into a unique alias.
//!
//! Seam note: `forge-build`'s `run()` constructs an `AtosSymbolicator` carrying
//! the executable it is about to launch, so the exe path flows from the build
//! side to the telemetry side even when the probe omits `meta.exe`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use forge_telemetry::Symbolicator;
use regex::Regex;

use crate::util::output_with_timeout;

// `atos` frame tail: `... (file.cpp:123)` (telemetry-server.ts:262).
static ATOS_FRAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(([^():]+):(\d+)\)\s*$").unwrap());

// Assignment LHS: `[type] lhs = ...` (telemetry-server.ts:310-312).
static ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:[\w:<>,~ \t*&\[\]]+?\s+)?((?:this->)?[A-Za-z_]\w*(?:(?:\.|->)[A-Za-z_]\w*)*)\s*=[^=]").unwrap()
});
// Declaration with constructor: `Type name(...)` / `Type name{...}` (`:315`).
static CTOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[\w:<>,~ \t*&]+\s+([A-Za-z_]\w*)\s*[({]").unwrap());

const KEYWORDS: &[&str] = &["if", "for", "while", "switch", "return", "else", "do", "case"];

/// Resolves probe buffer names to source variable names via `atos`.
pub struct AtosSymbolicator {
    /// Executable to symbolicate against when the probe omits `meta.exe`.
    fallback_exe: PathBuf,
    /// `path → lines` cache mirroring the TS `sourceCache` (`None` = unreadable).
    source_cache: Mutex<HashMap<PathBuf, Option<Vec<String>>>>,
}

impl AtosSymbolicator {
    pub fn new(fallback_exe: PathBuf) -> Self {
        AtosSymbolicator {
            fallback_exe,
            source_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Read `root/base_name`:`line_no` and extract the variable the allocation
    /// was assigned to (telemetry-server.ts `extractVarName`).
    fn extract_var_name(
        &self,
        roots: &[PathBuf],
        base_name: &str,
        line_no: usize,
    ) -> Option<String> {
        for root in roots {
            let p = root.join(base_name);
            let line = {
                let mut cache = self.source_cache.lock().unwrap();
                let content = cache.entry(p.clone()).or_insert_with(|| {
                    std::fs::read_to_string(&p)
                        .ok()
                        .map(|s| s.split('\n').map(str::to_string).collect())
                });
                match content {
                    Some(lines) => lines.get(line_no.wrapping_sub(1)).cloned(),
                    None => None,
                }
            };
            let Some(line) = line else { continue };
            if line.is_empty() {
                continue;
            }

            // Assignment: `[type] lhs = ...` → dotted identifier chain.
            if let Some(c) = ASSIGN_RE.captures(&line) {
                let name = &c[1];
                if !KEYWORDS.contains(&name) {
                    let stripped = name.strip_prefix("this->").unwrap_or(name);
                    return Some(stripped.replace("->", "."));
                }
            }
            // Declaration with constructor: `Type name(...)` / `Type name{...}`.
            if let Some(c) = CTOR_RE.captures(&line) {
                let name = &c[1];
                if !KEYWORDS.contains(&name) {
                    return Some(name.to_string());
                }
            }
        }
        None
    }
}

impl Symbolicator for AtosSymbolicator {
    fn variable_names(&self, addrs: &[String], exe: Option<&str>, load: &str) -> Vec<String> {
        if addrs.is_empty() {
            return Vec::new();
        }
        let exe: PathBuf = exe.map(PathBuf::from).unwrap_or_else(|| self.fallback_exe.clone());

        // `atos -o <exe> -l <load> <addrs...>`, 8s timeout (telemetry-server.ts:246-253).
        let mut cmd = Command::new("atos");
        cmd.arg("-o").arg(&exe).arg("-l").arg(load).args(addrs);
        let Some(out) = output_with_timeout(cmd, Duration::from_secs(8)) else {
            return Vec::new();
        };
        if !out.status.success() {
            return Vec::new();
        }
        let stdout = String::from_utf8_lossy(&out.stdout);

        // Search roots: the exe's directory and its parent (CMake build dirs
        // live inside the project root) (telemetry-server.ts:257).
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(d) = exe.parent() {
            roots.push(d.to_path_buf());
            if let Some(dd) = d.parent() {
                roots.push(dd.to_path_buf());
            }
        }

        let mut parts = Vec::new();
        for line in stdout.trim().split('\n') {
            if let Some(c) = ATOS_FRAME_RE.captures(line) {
                let base = &c[1];
                let line_no: usize = c[2].parse().unwrap_or(0);
                if let Some(name) = self.extract_var_name(&roots, base, line_no) {
                    parts.push(name);
                }
            }
        }
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sym(root: &Path) -> AtosSymbolicator {
        AtosSymbolicator::new(root.join("app"))
    }

    #[test]
    fn extract_assignment_variable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("model.cpp"),
            "int a;\nfloat* weights = allocate(1024);\nreturn weights;\n",
        )
        .unwrap();
        let s = sym(dir.path());
        let roots = vec![dir.path().to_path_buf()];
        assert_eq!(
            s.extract_var_name(&roots, "model.cpp", 2).as_deref(),
            Some("weights")
        );
    }

    #[test]
    fn extract_constructor_variable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("m.cpp"),
            "// header\nMatrix attn(rows, cols);\n",
        )
        .unwrap();
        let s = sym(dir.path());
        let roots = vec![dir.path().to_path_buf()];
        assert_eq!(
            s.extract_var_name(&roots, "m.cpp", 2).as_deref(),
            Some("attn")
        );
    }

    #[test]
    fn member_assignment_flattens_arrow_and_this() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.cpp"), "x;\nthis->cache->buf = make();\n").unwrap();
        let s = sym(dir.path());
        let roots = vec![dir.path().to_path_buf()];
        assert_eq!(
            s.extract_var_name(&roots, "s.cpp", 2).as_deref(),
            Some("cache.buf")
        );
    }

    #[test]
    fn control_flow_and_keyword_lines_yield_no_name() {
        let dir = tempfile::tempdir().unwrap();
        // `for (...)`: the `(` blocks the type prefix, so no variable is
        // recovered — control-flow lines don't name allocations.
        std::fs::write(dir.path().join("k.cpp"), "x;\nfor (int i = 0; i < n; i++) {\n").unwrap();
        // `do = ...`: assignment form captures the keyword `do`, which the
        // keyword filter rejects (telemetry-server.ts:308,313).
        std::fs::write(dir.path().join("d.cpp"), "x;\ndo = spin();\n").unwrap();
        let s = sym(dir.path());
        let roots = vec![dir.path().to_path_buf()];
        assert_eq!(s.extract_var_name(&roots, "k.cpp", 2), None);
        assert_eq!(s.extract_var_name(&roots, "d.cpp", 2), None);
    }

    #[test]
    fn missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let s = sym(dir.path());
        let roots = vec![PathBuf::from("/nonexistent")];
        assert_eq!(s.extract_var_name(&roots, "nope.cpp", 1), None);
    }
}
