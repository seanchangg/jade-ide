//! Text parsers ported from `src/main/build-runner.ts`: compiler/CMake
//! diagnostics, the `__FORGE_*` wire lines, AddressSanitizer output, the malloc
//! interposer's heap summary, and gcov coverage. Every regex here is a faithful
//! port of the corresponding JS `RegExp`; deviations are noted inline.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use forge_telemetry::{Scalar, Timing};
use regex::Regex;

use crate::types::*;

// ── Compiler & CMake diagnostics (build-runner.ts:50-79) ──

static COMPILER_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `file:line:col: severity: message` — build-runner.ts:53
    Regex::new(r"(?m)^(.+?):(\d+):(\d+):\s+(error|warning|note):\s+(.+)$").unwrap()
});

// CMake header line. The TS uses a single multiline regex with a lookahead
// terminator (`(?=\n\n|\n?$)`); the `regex` crate has no lookahead, so we match
// the header line here and gather the message lines that follow in code — same
// result (build-runner.ts:67).
static CMAKE_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^CMake (Error|Warning)(?: \(dev\))? at (.+?):(\d+)(?:\s+\([^)]*\))?:\s*$").unwrap()
});

static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

fn resolve(cwd: &Path, file: &str) -> PathBuf {
    let p = Path::new(file);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(file)
    }
}

/// Parse compiler diagnostics and CMake configure errors out of a build/config
/// log (build-runner.ts `parseCompilerErrors`).
pub fn parse_compiler_errors(output: &str, cwd: &Path) -> Vec<BuildError> {
    let mut errors = Vec::new();

    for c in COMPILER_RE.captures_iter(output) {
        let severity = match &c[4] {
            "error" => Severity::Error,
            "warning" => Severity::Warning,
            _ => Severity::Note,
        };
        errors.push(BuildError {
            file: resolve(cwd, &c[1]),
            line: c[2].parse().unwrap_or(0),
            column: c[3].parse().unwrap_or(0),
            message: c[5].to_string(),
            severity,
        });
    }

    // CMake errors: header line, then the immediately-following non-blank lines
    // form the message (collapsed whitespace), matching the TS lookahead.
    let lines: Vec<&str> = output.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if let Some(c) = CMAKE_HEADER_RE.captures(lines[i]) {
            let severity = if &c[1] == "Error" {
                Severity::Error
            } else {
                Severity::Warning
            };
            let mut msg_lines = Vec::new();
            let mut j = i + 1;
            while j < lines.len() && !lines[j].trim().is_empty() {
                msg_lines.push(lines[j]);
                j += 1;
            }
            let message = WS_RE
                .replace_all(msg_lines.join(" ").trim(), " ")
                .to_string();
            errors.push(BuildError {
                file: resolve(cwd, &c[2]),
                line: c[3].parse().unwrap_or(0),
                column: 1,
                message,
                severity,
            });
            i = j;
        } else {
            i += 1;
        }
    }

    errors
}

// ── `__FORGE_*` stdout lines (build-runner.ts:83-143) ──

/// `__FORGE_ALLOC|ptr|size|file|line|ts` / `__FORGE_FREE|…` (needs ≥6 fields).
pub fn parse_alloc_free(line: &str) -> Option<AllocEvent> {
    let kind = if line.starts_with("__FORGE_ALLOC|") {
        AllocKind::Alloc
    } else if line.starts_with("__FORGE_FREE|") {
        AllocKind::Free
    } else {
        return None;
    };
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 6 {
        return None;
    }
    Some(AllocEvent {
        kind,
        pointer: parts[1].to_string(),
        size: parts[2].parse().unwrap_or(0),
        file: parts[3].to_string(),
        line: parts[4].parse().unwrap_or(0),
        timestamp: parts[5].parse().unwrap_or(0.0),
    })
}

/// `__FORGE_SCALAR|name|step|value|ts` (needs ≥5 fields).
pub fn parse_scalar(line: &str) -> Option<Scalar> {
    if !line.starts_with("__FORGE_SCALAR|") {
        return None;
    }
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 5 {
        return None;
    }
    Some(Scalar {
        name: parts[1].to_string(),
        step: parts[2].parse().unwrap_or(0),
        value: parts[3].parse().unwrap_or(0.0),
        t: Some(parts[4].parse().unwrap_or(0.0)),
    })
}

/// `__FORGE_TIMING|name|duration_ms|step` (needs ≥4 fields).
pub fn parse_timing(line: &str) -> Option<Timing> {
    if !line.starts_with("__FORGE_TIMING|") {
        return None;
    }
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 4 {
        return None;
    }
    Some(Timing {
        name: parts[1].to_string(),
        ms: parts[2].parse().unwrap_or(0.0),
        step: parts[3].parse().unwrap_or(0),
    })
}

/// `__FORGE_HEAP_SUMMARY|total_alloc|total_freed|current_heap|peak_heap|alloc_count|free_count`
/// (needs ≥7 fields; build-runner.ts `parseHeapSummary`). The two stray
/// `console.log`s on this path (perf-findings.md #2) are intentionally omitted.
pub fn parse_heap_summary(line: &str) -> Option<MemoryEvent> {
    if !line.starts_with("__FORGE_HEAP_SUMMARY|") {
        return None;
    }
    let parts: Vec<&str> = line.split('|').collect();
    if parts.len() < 7 {
        return None;
    }
    let n = |s: &str| s.parse::<i64>().unwrap_or(0);
    Some(MemoryEvent::HeapSummary {
        total_alloc: n(parts[1]),
        total_freed: n(parts[2]),
        current_heap: n(parts[3]),
        peak_heap: n(parts[4]),
        alloc_count: n(parts[5]),
        free_count: n(parts[6]),
    })
}

// ── AddressSanitizer (build-runner.ts:146-218) ──

static ASAN_SUMMARY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"SUMMARY:\s*AddressSanitizer:\s*(\d+)\s*byte\(s\)\s*leaked\s*in\s*(\d+)\s*allocation\(s\)").unwrap()
});
static ASAN_LEAK_ENTRY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)#\d+\s+0x[0-9a-f]+\s+in\s+(\S+)\s+(\S+?):(\d+)").unwrap()
});
static ASAN_ERROR_TYPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"ERROR:\s*AddressSanitizer:\s*([\w-]+)").unwrap());

/// Parse ASan leak output. Returns memory events (summary + per-leak locations)
/// plus red `[ASan] <type>` output lines for any reported error types
/// (build-runner.ts `parseAsanOutput`).
pub fn parse_asan_output(output: &str) -> (Vec<MemoryEvent>, Vec<String>) {
    let mut events = Vec::new();
    let mut outputs = Vec::new();

    if let Some(c) = ASAN_SUMMARY_RE.captures(output) {
        events.push(MemoryEvent::AsanLeakSummary {
            leaked_bytes: c[1].parse().unwrap_or(0),
            leaked_allocations: c[2].parse().unwrap_or(0),
        });
    }

    for c in ASAN_LEAK_ENTRY_RE.captures_iter(output) {
        events.push(MemoryEvent::AsanLeakLocation {
            function_name: c[1].to_string(),
            file: c[2].to_string(),
            line: c[3].parse().unwrap_or(0),
        });
    }

    for c in ASAN_ERROR_TYPE_RE.captures_iter(output) {
        outputs.push(format!("\x1b[31m[ASan] {}\x1b[0m\n", &c[1]));
    }

    (events, outputs)
}

static ASAN_STATS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Stats:\s*(\d+)M?\s*mallocs?,\s*(\d+)F?\s*frees?,\s*(\d+)\s*total").unwrap()
});
static ASAN_ALLOC_COUNT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"number of allocations\s*:\s*(\d+)").unwrap());
static ASAN_FREE_COUNT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"number of deallocations\s*:\s*(\d+)").unwrap());
static ASAN_BYTES_ALLOC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"bytes allocated\s*:\s*(\d+)").unwrap());
static ASAN_BYTES_FREED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"bytes freed\s*:\s*(\d+)").unwrap());

/// Parse ASan allocation stats. Handles both the compact `Stats:` line and the
/// `print_stats=1` key/value block; either can produce an event, matching the
/// TS which sends one per format found (build-runner.ts `parseAsanStats`).
pub fn parse_asan_stats(output: &str) -> Vec<MemoryEvent> {
    let mut events = Vec::new();

    if let Some(c) = ASAN_STATS_RE.captures(output) {
        events.push(MemoryEvent::AsanStats {
            total_allocations: c[1].parse().unwrap_or(0),
            total_frees: c[2].parse().unwrap_or(0),
            total_bytes: c[3].parse().unwrap_or(0),
            total_freed_bytes: None,
        });
    }

    let alloc = ASAN_ALLOC_COUNT_RE.captures(output);
    let free = ASAN_FREE_COUNT_RE.captures(output);
    let bytes = ASAN_BYTES_ALLOC_RE.captures(output);
    let freed = ASAN_BYTES_FREED_RE.captures(output);
    if alloc.is_some() || free.is_some() || bytes.is_some() {
        let g = |c: Option<regex::Captures>| c.map(|c| c[1].parse().unwrap_or(0)).unwrap_or(0);
        events.push(MemoryEvent::AsanStats {
            total_allocations: g(alloc),
            total_frees: g(free),
            total_bytes: g(bytes),
            total_freed_bytes: Some(g(freed)),
        });
    }

    events
}

// ── gcov coverage (build-runner.ts:722-734) ──

static GCOV_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\d+):\s*(\d+):").unwrap());

/// Parse `.gcov` content into `(line, count)` pairs, keeping only executed
/// lines (`count > 0 && line > 0`), matching the TS filter.
pub fn parse_gcov(content: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for line in content.lines() {
        if let Some(c) = GCOV_RE.captures(line) {
            let count: u32 = c[1].parse().unwrap_or(0);
            let line_no: u32 = c[2].parse().unwrap_or(0);
            if count > 0 && line_no > 0 {
                out.push((line_no, count));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn compiler_errors_parse_file_line_col_severity() {
        let out = "\
/proj/main.cpp:10:5: error: use of undeclared identifier 'foo'
main.cpp:3:1: warning: unused variable 'x'
main.cpp:3:1: note: expanded from macro";
        let errors = parse_compiler_errors(out, Path::new("/proj"));
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0].file, PathBuf::from("/proj/main.cpp"));
        assert_eq!(errors[0].line, 10);
        assert_eq!(errors[0].column, 5);
        assert_eq!(errors[0].severity, Severity::Error);
        assert_eq!(errors[0].message, "use of undeclared identifier 'foo'");
        // relative path is joined to cwd
        assert_eq!(errors[1].file, PathBuf::from("/proj/main.cpp"));
        assert_eq!(errors[1].severity, Severity::Warning);
        assert_eq!(errors[2].severity, Severity::Note);
    }

    #[test]
    fn cmake_configure_error_parses_header_and_message() {
        let out = "\
CMake Error at CMakeLists.txt:12 (add_executable):
  Cannot find source file:

    nope.cpp

Some trailing noise";
        let errors = parse_compiler_errors(out, Path::new("/proj"));
        let cmake: Vec<_> = errors
            .iter()
            .filter(|e| e.file.ends_with("CMakeLists.txt"))
            .collect();
        assert_eq!(cmake.len(), 1);
        assert_eq!(cmake[0].line, 12);
        assert_eq!(cmake[0].column, 1);
        assert_eq!(cmake[0].severity, Severity::Error);
        assert_eq!(cmake[0].message, "Cannot find source file:");
    }

    #[test]
    fn cmake_dev_warning_variant() {
        let out = "CMake Warning (dev) at CMakeLists.txt:5 (project):\n  Policy CMP0000 warning\n";
        let errors = parse_compiler_errors(out, Path::new("/p"));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].severity, Severity::Warning);
        assert_eq!(errors[0].line, 5);
        assert_eq!(errors[0].message, "Policy CMP0000 warning");
    }

    #[test]
    fn alloc_and_free_lines() {
        let a = parse_alloc_free("__FORGE_ALLOC|0xdead|64|main.cpp|10|1234.5").unwrap();
        assert_eq!(a.kind, AllocKind::Alloc);
        assert_eq!(a.pointer, "0xdead");
        assert_eq!(a.size, 64);
        assert_eq!(a.file, "main.cpp");
        assert_eq!(a.line, 10);
        assert_eq!(a.timestamp, 1234.5);
        let f = parse_alloc_free("__FORGE_FREE|0xdead|64|main.cpp|10|1240.0").unwrap();
        assert_eq!(f.kind, AllocKind::Free);
        // too few fields -> None
        assert!(parse_alloc_free("__FORGE_ALLOC|0xdead|64").is_none());
        assert!(parse_alloc_free("regular output").is_none());
    }

    #[test]
    fn scalar_and_timing_lines() {
        let s = parse_scalar("__FORGE_SCALAR|loss|3|0.125|1000.0").unwrap();
        assert_eq!(s.name, "loss");
        assert_eq!(s.step, 3);
        assert_eq!(s.value, 0.125);
        assert_eq!(s.t, Some(1000.0));
        assert!(parse_scalar("__FORGE_SCALAR|loss|3").is_none());

        let t = parse_timing("__FORGE_TIMING|forward|12.4|7").unwrap();
        assert_eq!(t.name, "forward");
        assert_eq!(t.ms, 12.4);
        assert_eq!(t.step, 7);
        assert!(parse_timing("__FORGE_TIMING|forward").is_none());
    }

    #[test]
    fn heap_summary_line() {
        let e = parse_heap_summary("__FORGE_HEAP_SUMMARY|1000|800|200|500|10|8").unwrap();
        match e {
            MemoryEvent::HeapSummary {
                total_alloc,
                total_freed,
                current_heap,
                peak_heap,
                alloc_count,
                free_count,
            } => {
                assert_eq!(
                    (
                        total_alloc,
                        total_freed,
                        current_heap,
                        peak_heap,
                        alloc_count,
                        free_count
                    ),
                    (1000, 800, 200, 500, 10, 8)
                );
            }
            _ => panic!("wrong variant"),
        }
        assert!(parse_heap_summary("__FORGE_HEAP_SUMMARY|1|2|3").is_none());
    }

    #[test]
    fn asan_leak_output() {
        let out = "\
==1234==ERROR: AddressSanitizer: heap-use-after-free on address 0x60200
    #0 0x1001 in main /proj/main.cpp:12
    #1 0x1002 in helper /proj/util.cpp:4

SUMMARY: AddressSanitizer: 64 byte(s) leaked in 2 allocation(s).";
        let (events, outputs) = parse_asan_output(out);
        let summary = events.iter().find(|e| matches!(e, MemoryEvent::AsanLeakSummary { .. }));
        match summary.unwrap() {
            MemoryEvent::AsanLeakSummary {
                leaked_bytes,
                leaked_allocations,
            } => {
                assert_eq!(*leaked_bytes, 64);
                assert_eq!(*leaked_allocations, 2);
            }
            _ => unreachable!(),
        }
        let locs: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, MemoryEvent::AsanLeakLocation { .. }))
            .collect();
        assert_eq!(locs.len(), 2);
        match locs[0] {
            MemoryEvent::AsanLeakLocation {
                function_name,
                file,
                line,
            } => {
                assert_eq!(function_name, "main");
                assert_eq!(file, "/proj/main.cpp");
                assert_eq!(*line, 12);
            }
            _ => unreachable!(),
        }
        assert_eq!(outputs.len(), 1);
        assert!(outputs[0].contains("[ASan] heap-use-after-free"));
    }

    #[test]
    fn asan_stats_compact_format() {
        let events = parse_asan_stats("Stats: 42 mallocs, 40 frees, 4096 total bytes");
        assert_eq!(events.len(), 1);
        match &events[0] {
            MemoryEvent::AsanStats {
                total_allocations,
                total_frees,
                total_bytes,
                total_freed_bytes,
            } => {
                assert_eq!(*total_allocations, 42);
                assert_eq!(*total_frees, 40);
                assert_eq!(*total_bytes, 4096);
                assert_eq!(*total_freed_bytes, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn asan_stats_print_stats_format() {
        let out = "\
Stats: 0 mallocs
number of allocations : 100
number of deallocations: 90
bytes allocated        : 8192
bytes freed            : 7000";
        let events = parse_asan_stats(out);
        // the print_stats block is the last event
        let last = events.last().unwrap();
        match last {
            MemoryEvent::AsanStats {
                total_allocations,
                total_frees,
                total_bytes,
                total_freed_bytes,
            } => {
                assert_eq!(*total_allocations, 100);
                assert_eq!(*total_frees, 90);
                assert_eq!(*total_bytes, 8192);
                assert_eq!(*total_freed_bytes, Some(7000));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn gcov_counts() {
        let content = "\
        -:    0:Source:main.cpp
        5:    1:int main() {
    #####:    2:  unreachable();
        3:    3:  return 0;
        -:    4:}";
        let pairs = parse_gcov(content);
        assert_eq!(pairs, vec![(1, 5), (3, 3)]);
    }
}
