//! Named benchmarks (feature inventory §5.4 BENCHMARKS).
//!
//! A benchmark is a saved snapshot of a completed run, created by clicking the
//! ⚑ flag on a HISTORY row. The serialized shape matches the Electron
//! `BenchmarkEntry` (`renderer/panels/runtime-panel.ts:12-19`, camelCase) so the
//! same `ui.benchmarks` array round-trips between the two apps.
//!
//! This module is the pure model + the sorting / fastest / delta-vs-latest math
//! (unit-tested); `panels::runtime_panel` renders it and `workspace_state`
//! persists it. Nothing here touches GPUI or the app.

use serde::{Deserialize, Serialize};

use crate::format::format_duration;

/// One saved benchmark (`BenchmarkEntry`). `duration` is milliseconds;
/// `peak_allocation` is bytes. Numeric types are wide/float so an Electron file
/// written with fractional `performance.now()` durations round-trips unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Benchmark {
    pub name: String,
    pub flags: String,
    pub duration: f64,
    pub peak_allocation: i64,
    pub alloc_count: u64,
    pub timestamp: i64,
}

/// `Date.now()` epoch-ms (benchmark `timestamp`).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The inline name-input prefill for a run being saved (§5.4): `#<run> <flags>`
/// (the flags suffix is dropped when empty). Matches `runtime-panel.ts:207`.
pub fn default_name(run_index: usize, flags: &str) -> String {
    let flags = flags.trim();
    if flags.is_empty() {
        format!("#{run_index}")
    } else {
        format!("#{run_index} {flags}")
    }
}

/// Indices into `benchmarks` sorted **fastest-first** (ascending duration). The
/// sort is stable so equal durations keep insertion order (`renderBenchmarks`
/// maps then sorts, `runtime-panel.ts:264-266`).
pub fn sorted_fastest_first(benchmarks: &[Benchmark]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..benchmarks.len()).collect();
    idx.sort_by(|&a, &b| {
        benchmarks[a]
            .duration
            .partial_cmp(&benchmarks[b].duration)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    idx
}

/// The fastest (minimum) duration among `benchmarks`, or `None` when empty. The
/// row matching this value gets the accent color (`runtime-panel.ts:268,284`).
pub fn fastest_duration(benchmarks: &[Benchmark]) -> Option<f64> {
    benchmarks
        .iter()
        .map(|b| b.duration)
        .fold(None, |acc, d| Some(acc.map_or(d, |m: f64| m.min(d))))
}

/// The delta tag of a benchmark vs the **latest run** (`runtime-panel.ts:296-311`).
/// `diff = last_run - benchmark`; `|diff| < 1ms` is `Equal`. A negative diff (the
/// benchmark is slower than the latest run) renders `↓` in the accent color; a
/// positive diff renders `↑` in the error color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Delta {
    /// No latest run to compare against.
    None,
    /// Within 1ms — rendered as `=`.
    Equal,
    /// `diff < 0` — `{fmt}↓`, accent color.
    Down(f64),
    /// `diff > 0` — `{fmt}↑`, error color.
    Up(f64),
}

impl Delta {
    /// The rendered tag text (`=` / `12ms↓` / `1.50s↑`), or `None` when there is
    /// no latest run.
    pub fn label(&self) -> Option<String> {
        match self {
            Delta::None => None,
            Delta::Equal => Some("=".to_string()),
            Delta::Down(v) => Some(format!("{}\u{2193}", format_duration(*v))),
            Delta::Up(v) => Some(format!("{}\u{2191}", format_duration(*v))),
        }
    }
}

/// Compute the delta of `bench_duration` vs the latest completed run's duration.
pub fn delta_vs_last(bench_duration: f64, last_run_ms: Option<f64>) -> Delta {
    let Some(last) = last_run_ms else {
        return Delta::None;
    };
    let diff = last - bench_duration;
    if diff.abs() < 1.0 {
        Delta::Equal
    } else if diff < 0.0 {
        Delta::Down(diff.abs())
    } else {
        Delta::Up(diff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench(name: &str, duration: f64) -> Benchmark {
        Benchmark {
            name: name.to_string(),
            flags: String::new(),
            duration,
            peak_allocation: 0,
            alloc_count: 0,
            timestamp: 0,
        }
    }

    #[test]
    fn default_name_prefill() {
        assert_eq!(default_name(3, ""), "#3");
        assert_eq!(default_name(3, "   "), "#3");
        assert_eq!(default_name(7, "-O3 -march=native"), "#7 -O3 -march=native");
    }

    #[test]
    fn sorts_fastest_first_stable() {
        let bms = vec![
            bench("a", 120.0),
            bench("b", 40.0),
            bench("c", 40.0), // ties with b → keeps insertion order (b before c)
            bench("d", 80.0),
        ];
        let order = sorted_fastest_first(&bms);
        assert_eq!(order, vec![1, 2, 3, 0]);
        assert_eq!(fastest_duration(&bms), Some(40.0));
    }

    #[test]
    fn fastest_empty() {
        assert_eq!(fastest_duration(&[]), None);
    }

    #[test]
    fn delta_math_and_labels() {
        // No latest run → None (no tag rendered).
        assert_eq!(delta_vs_last(50.0, None), Delta::None);
        assert!(delta_vs_last(50.0, None).label().is_none());

        // Within 1ms → Equal.
        assert_eq!(delta_vs_last(50.4, Some(50.0)), Delta::Equal);
        assert_eq!(delta_vs_last(50.0, Some(50.0)).label().as_deref(), Some("="));

        // Benchmark slower than latest run (diff = 100 - 150 = -50) → Down, accent.
        assert_eq!(delta_vs_last(150.0, Some(100.0)), Delta::Down(50.0));
        assert_eq!(
            delta_vs_last(150.0, Some(100.0)).label().as_deref(),
            Some("50ms\u{2193}")
        );

        // Benchmark faster than latest run (diff = 200 - 120 = +80) → Up, error.
        assert_eq!(delta_vs_last(120.0, Some(200.0)), Delta::Up(80.0));
        assert_eq!(
            delta_vs_last(120.0, Some(200.0)).label().as_deref(),
            Some("80ms\u{2191}")
        );
    }
}
