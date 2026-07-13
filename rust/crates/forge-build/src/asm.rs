//! `clang++ -S -masm=intel` assembly generation with `.loc`-directive
//! asm→source line mapping and Godbolt-style filtering (build-runner.ts:809-903).

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::types::AsmResult;
use crate::util::output_with_stdin_timeout;

static LOC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\.loc\s+\d+\s+(\d+)").unwrap());
static BRANCH_LABEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^L[A-Za-z]+\d").unwrap());

/// Demangle C++ symbols with `c++filt` in one pass, falling back to the mangled
/// text if `c++filt` is unavailable (build-runner.ts `demangleCpp`, 5s timeout).
fn demangle(asm: &str) -> String {
    match output_with_stdin_timeout(Command::new("c++filt"), asm, Duration::from_secs(5)) {
        Some(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => asm.to_string(),
    }
}

/// Filter raw `clang++ -S` output to readable assembly and build the
/// (1-based filtered-line → source-line) map. Pure; unit-testable.
pub fn filter_asm(raw: &str) -> (Vec<String>, HashMap<usize, u32>) {
    let mut filtered: Vec<String> = Vec::new();
    let mut asm_to_source: HashMap<usize, u32> = HashMap::new();
    let mut current_source_line: u32 = 0;

    const LABEL_SKIP: &[&str] = &["Lfunc_", "Ltmp", "Lcfi", "Lloh", "LCPI", ".Lfunc_", ".Ltmp"];

    for line in raw.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // `.loc filenum lineno [column]` — update mapping, don't emit.
        if let Some(c) = LOC_RE.captures(trimmed) {
            current_source_line = c[1].parse().unwrap_or(0);
            continue;
        }

        // Labels (skip debug/internal ones).
        if trimmed.ends_with(':') && !LABEL_SKIP.iter().any(|p| trimmed.starts_with(p)) {
            let is_branch_label = trimmed.starts_with('.') || BRANCH_LABEL_RE.is_match(trimmed);
            if !is_branch_label && !filtered.is_empty() {
                filtered.push(String::new());
            }
            filtered.push(line.to_string());
            continue;
        }

        // Instructions (indented, not directives or debug noise).
        if line.starts_with('\t')
            && !trimmed.starts_with('.')
            && !trimmed.starts_with(";DEBUG_VALUE")
            && !trimmed.starts_with("; %")
            && !trimmed.starts_with(";;")
            && !trimmed.starts_with(".loh")
        {
            filtered.push(line.to_string());
            if current_source_line > 0 {
                asm_to_source.insert(filtered.len(), current_source_line);
            }
            continue;
        }

        // Data definitions and string literals.
        const DATA_DIRECTIVES: &[&str] = &[
            ".long", ".quad", ".byte", ".short", ".zero", ".ascii", ".asciz", ".string", ".space",
            ".set",
        ];
        if DATA_DIRECTIVES.iter().any(|d| trimmed.starts_with(d)) {
            filtered.push(line.to_string());
            if current_source_line > 0 {
                asm_to_source.insert(filtered.len(), current_source_line);
            }
            continue;
        }
    }

    (filtered, asm_to_source)
}

/// Generate assembly for `file` (build-runner.ts `generateAssembly`).
pub async fn generate_asm(
    file: &Path,
    forge_include_dir: &Path,
    extra_flags: &[String],
) -> AsmResult {
    let cwd = file.parent().unwrap_or_else(|| Path::new("."));
    let mut cmd = tokio::process::Command::new("clang++");
    cmd.current_dir(cwd)
        .args(["-std=c++17", "-O3", "-march=native", "-g"])
        .arg(format!("-I{}", forge_include_dir.display()))
        .args(extra_flags)
        .args([
            "-S",
            "-masm=intel",
            "-fno-asynchronous-unwind-tables",
            "-fno-unwind-tables",
            "-o",
            "-",
        ])
        .arg(file);

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            return AsmResult {
                success: false,
                asm: String::new(),
                error: Some(e.to_string()),
                asm_to_source: HashMap::new(),
            }
        }
    };

    if !output.status.success() {
        return AsmResult {
            success: false,
            asm: String::new(),
            error: Some(String::from_utf8_lossy(&output.stderr).into_owned()),
            asm_to_source: HashMap::new(),
        };
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let (filtered, asm_to_source) = filter_asm(&raw);
    let demangled = demangle(&filtered.join("\n"));
    AsmResult {
        success: true,
        asm: demangled,
        error: None,
        asm_to_source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loc_directives_map_instructions_to_source_lines() {
        let raw = "\
\t.section\t__TEXT
_main:
\t.loc\t1 5 0
\tmov\teax, 1
\tadd\teax, 2
\t.loc\t1 7 0
\tret
Ltmp0:
\t.loc\t1 9 0
\tnop";
        let (filtered, map) = filter_asm(raw);
        // `_main:` is a function label → blank separator is only added when
        // filtered is non-empty; it's first so no leading blank.
        assert_eq!(filtered[0], "_main:");
        // instructions kept; directives (.section) and Ltmp0: dropped.
        assert!(filtered.iter().any(|l| l.trim() == "mov\teax, 1".trim()));
        assert!(!filtered.iter().any(|l| l.contains(".section")));
        assert!(!filtered.iter().any(|l| l.trim_end().ends_with("Ltmp0:")));
        // `mov` is filtered line 2 (after `_main:`), mapped to source line 5.
        let mov_idx = filtered.iter().position(|l| l.contains("mov")).unwrap() + 1;
        assert_eq!(map.get(&mov_idx), Some(&5));
        let ret_idx = filtered.iter().position(|l| l.trim() == "ret").unwrap() + 1;
        assert_eq!(map.get(&ret_idx), Some(&7));
        // nop after Ltmp0 label picks up loc line 9
        let nop_idx = filtered.iter().position(|l| l.trim() == "nop").unwrap() + 1;
        assert_eq!(map.get(&nop_idx), Some(&9));
    }

    #[test]
    fn internal_labels_filtered_but_data_kept() {
        let raw = "\
LCPI0_0:
\t.quad\t4614256656552045848
Lfunc_end0:
_foo:
\tret";
        let (filtered, _map) = filter_asm(raw);
        assert!(!filtered.iter().any(|l| l.contains("LCPI0_0:")));
        assert!(!filtered.iter().any(|l| l.contains("Lfunc_end0:")));
        assert!(filtered.iter().any(|l| l.trim() == ".quad\t4614256656552045848".trim()));
        assert!(filtered.iter().any(|l| l == "_foo:"));
    }
}
