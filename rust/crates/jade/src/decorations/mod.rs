//! Editor decoration systems (Phase-4 wave 3): the two flagship editor
//! decoration ports from the Electron renderer onto the read-only viewer.
//!
//!   - [`size_annotations`] — static size scanner (§4.5 system 1).
//!   - [`exec_annotations`] — execution-count formatting + diff arrows
//!     (§4.5 system 2).
//!   - [`flow`] — execution-flow analyzer (§4.8).
//!
//! The runtime-allocation decorations (§4.5 system 3) are fed by the existing
//! [`crate::memory_bar::MemoryBarState`] per-`file:line` tracker; the trailing
//! merged annotation string is composed by [`merge_annotation`] and rendered by
//! `panels::code_view`.

pub mod exec_annotations;
pub mod flow;
pub mod size_annotations;

use crate::format::format_bytes;

/// Compose the ONE trailing end-of-line annotation from its three parts, in the
/// order the spec fixes: static size, execution count, runtime allocation
/// (feature inventory §4.5; deliverable §4 merged example
/// `  [24 B]  ×1.5K↑  ← 3 calls, 1.2KB`). Each present part is separated by two
/// spaces, and the whole run is prefixed with two spaces so it sits off the code.
/// Returns `None` when no part applies.
pub fn merge_annotation(
    size: Option<&str>,
    exec: Option<&str>,
    runtime: Option<&RuntimeAlloc>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = size {
        parts.push(s.to_string());
    }
    if let Some(e) = exec {
        parts.push(e.to_string());
    }
    if let Some(r) = runtime {
        parts.push(r.render());
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("  {}", parts.join("  ")))
}

/// Runtime allocation roll-up for a single `file:line` (§4.5 system 3, port of
/// `memory-decorations.ts:568-605`). Fed from the [`crate::memory_bar`] tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeAlloc {
    pub calls: i64,
    pub bytes: i64,
    pub leaked: i64,
}

impl RuntimeAlloc {
    /// `← N calls, X.YKB[, M leaked]` (`:582-589`).
    pub fn render(&self) -> String {
        let mut parts = vec![format!("{} calls", self.calls), format_bytes(self.bytes as f64)];
        if self.leaked > 0 {
            parts.push(format!("{} leaked", self.leaked));
        }
        format!("\u{2190} {}", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_render_with_and_without_leak() {
        let r = RuntimeAlloc {
            calls: 3,
            bytes: 1229,
            leaked: 0,
        };
        assert_eq!(r.render(), "\u{2190} 3 calls, 1.2KB");
        let r2 = RuntimeAlloc {
            calls: 3,
            bytes: 1229,
            leaked: 2,
        };
        assert_eq!(r2.render(), "\u{2190} 3 calls, 1.2KB, 2 leaked");
    }

    #[test]
    fn merge_composes_all_three() {
        let rt = RuntimeAlloc {
            calls: 3,
            bytes: 1229,
            leaked: 0,
        };
        let merged =
            merge_annotation(Some("[24 B]"), Some("\u{00d7}1.5K\u{2191}"), Some(&rt)).unwrap();
        assert_eq!(merged, "  [24 B]  \u{00d7}1.5K\u{2191}  \u{2190} 3 calls, 1.2KB");
    }

    #[test]
    fn merge_partial_and_empty() {
        assert_eq!(merge_annotation(Some("[8 B]"), None, None).as_deref(), Some("  [8 B]"));
        assert_eq!(
            merge_annotation(None, Some("\u{00d7}5"), None).as_deref(),
            Some("  \u{00d7}5")
        );
        assert_eq!(merge_annotation(None, None, None), None);
    }
}
