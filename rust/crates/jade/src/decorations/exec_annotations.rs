//! Execution-count annotations (feature inventory §4.5 system 2, port of
//! `memory-decorations.ts` `formatExecCount`/`collectExecAnnotations`,
//! `:474-495`).
//!
//! From the last run's per-line execution counts: only `count > 1`; format
//! `×123 / ×1.5K / ×2.3M`; and a `↑`/`↓` diff arrow versus the **previous run's
//! snapshot**. Snapshot rotation (previous ← current on run completion) is wired
//! in `app.rs::on_run_done` — see [`crate::app::JadeApp::prev_executed`].

use std::collections::HashMap;

const MULT: char = '\u{00d7}'; // ×
const UP: char = '\u{2191}'; // ↑
const DOWN: char = '\u{2193}'; // ↓

/// Format an execution count: `×123`, `×1.5K` (≥1e3), `×2.3M` (≥1e6) (`:474-478`).
pub fn format_exec_count(count: u32) -> String {
    if count >= 1_000_000 {
        format!("{MULT}{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{MULT}{:.1}K", count as f64 / 1_000.0)
    } else {
        format!("{MULT}{count}")
    }
}

/// The exec annotation for a single line (or `None` when `count <= 1`).
/// `prev` is the previous run's count for the same line (0 if none): a `↑` is
/// appended when the count rose and `↓` when it fell, but only once a previous
/// run exists (`prev > 0`), matching `:488-493`.
pub fn annotate_line(count: u32, prev: u32) -> Option<String> {
    if count <= 1 {
        return None;
    }
    let suffix = if prev > 0 && count > prev {
        UP.to_string()
    } else if prev > 0 && count < prev {
        DOWN.to_string()
    } else {
        String::new()
    };
    Some(format!("{}{}", format_exec_count(count), suffix))
}

/// Collect exec annotations for every qualifying line (`:480-495`). `executed`
/// is the current run's counts; `prev` is the previous run's snapshot.
pub fn collect_exec_annotations(
    executed: &HashMap<u32, u32>,
    prev: &HashMap<u32, u32>,
) -> HashMap<usize, String> {
    let mut out = HashMap::new();
    for (&line, &count) in executed {
        let p = prev.get(&line).copied().unwrap_or(0);
        if let Some(text) = annotate_line(count, p) {
            out.insert(line as usize, text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_scales() {
        assert_eq!(format_exec_count(1), "\u{00d7}1");
        assert_eq!(format_exec_count(123), "\u{00d7}123");
        assert_eq!(format_exec_count(1_500), "\u{00d7}1.5K");
        assert_eq!(format_exec_count(2_300_000), "\u{00d7}2.3M");
        assert_eq!(format_exec_count(999), "\u{00d7}999");
    }

    #[test]
    fn count_one_is_suppressed() {
        assert_eq!(annotate_line(1, 0), None);
        assert_eq!(annotate_line(0, 0), None);
    }

    #[test]
    fn arrows_versus_previous_run() {
        // No previous run → no arrow.
        assert_eq!(annotate_line(10, 0).unwrap(), "\u{00d7}10");
        // Rose vs previous.
        assert_eq!(annotate_line(1500, 1000).unwrap(), "\u{00d7}1.5K\u{2191}");
        // Fell vs previous.
        assert_eq!(annotate_line(1000, 1500).unwrap(), "\u{00d7}1.0K\u{2193}");
        // Unchanged → no arrow.
        assert_eq!(annotate_line(1000, 1000).unwrap(), "\u{00d7}1.0K");
    }

    #[test]
    fn collect_merges_and_filters() {
        let mut executed = HashMap::new();
        executed.insert(10u32, 5u32);
        executed.insert(11u32, 1u32); // filtered (count == 1)
        executed.insert(12u32, 2_300_000u32);
        let mut prev = HashMap::new();
        prev.insert(12u32, 1_000_000u32); // rose

        let out = collect_exec_annotations(&executed, &prev);
        assert_eq!(out.get(&10).map(String::as_str), Some("\u{00d7}5"));
        assert_eq!(out.get(&11), None);
        assert_eq!(out.get(&12).map(String::as_str), Some("\u{00d7}2.3M\u{2191}"));
    }
}
