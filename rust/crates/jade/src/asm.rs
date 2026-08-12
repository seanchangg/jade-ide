//! ASM viewer model (feature inventory §6 "ASM viewer" / app.ts:304-445).
//!
//! Holds the `-O3 -march=native` assembly of the active file plus the
//! bidirectional line map from `jade_build::generate_asm` (which builds it from
//! the compiler's `.loc` directives). The viewer highlights, for the source
//! caret line, every mapped asm line — and, on an asm click, scrolls + highlights
//! the counterpart source line (asm→src also scrolls).
//!
//! `jade_build::AsmResult::asm_to_source` keys are **1-based filtered-asm line
//! numbers**; values are **1-based source lines**. This module keeps a reverse
//! index (source line → the 0-based asm row indices) for the source→asm
//! highlight direction, and exposes pure lookups that are unit-tested.

use std::collections::HashMap;

/// The rendered ASM state (right-half overlay).
pub struct AsmView {
    /// Assembly, one entry per line (monospace, read-only, virtualized).
    pub lines: Vec<String>,
    /// 1-based asm line → 1-based source line (from the compiler `.loc` map).
    asm_to_source: HashMap<usize, u32>,
    /// 1-based source line → the **0-based** asm row indices that map to it.
    source_to_asm: HashMap<u32, Vec<usize>>,
    /// The source line whose asm rows are currently highlighted (1-based), set
    /// either by the source caret (auto) or by clicking an asm row.
    pub selected_source: Option<u32>,
}

impl AsmView {
    /// Build from the engine's asm text + `asm_to_source` map. The reverse index
    /// is derived here once; asm rows are 0-based (uniform-list indices), the map
    /// keys are 1-based, so we subtract one.
    pub fn new(asm: &str, asm_to_source: HashMap<usize, u32>) -> AsmView {
        let lines: Vec<String> = if asm.is_empty() {
            Vec::new()
        } else {
            asm.split('\n').map(str::to_string).collect()
        };
        let mut source_to_asm: HashMap<u32, Vec<usize>> = HashMap::new();
        for (&asm_line_1, &src) in &asm_to_source {
            if asm_line_1 == 0 {
                continue;
            }
            source_to_asm.entry(src).or_default().push(asm_line_1 - 1);
        }
        for v in source_to_asm.values_mut() {
            v.sort_unstable();
        }
        AsmView {
            lines,
            asm_to_source,
            source_to_asm,
            selected_source: None,
        }
    }

    /// The source line (1-based) an asm row (0-based) maps to, if any.
    pub fn source_for_asm(&self, asm_row0: usize) -> Option<u32> {
        self.asm_to_source.get(&(asm_row0 + 1)).copied()
    }

    /// The asm rows (0-based) that map to a source line (1-based). Empty when the
    /// source line produced no code (e.g. a comment or a folded-away line).
    pub fn asm_rows_for_source(&self, source_line: u32) -> &[usize] {
        self.source_to_asm
            .get(&source_line)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The first asm row (0-based) for a source line — the scroll target when the
    /// source caret moves (asm→src also scrolls; app.ts:399-407).
    pub fn first_asm_row_for_source(&self, source_line: u32) -> Option<usize> {
        self.asm_rows_for_source(source_line).first().copied()
    }

    /// True when asm row `asm_row0` should be highlighted for the current
    /// selection (its source line equals the selected source line).
    pub fn asm_row_highlighted(&self, asm_row0: usize) -> bool {
        match (self.selected_source, self.source_for_asm(asm_row0)) {
            (Some(sel), Some(src)) => sel == src,
            _ => false,
        }
    }

    /// Set the highlighted source line (from the source caret).
    pub fn select_source(&mut self, source_line: u32) {
        self.selected_source = Some(source_line);
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AsmView {
        // 4 asm lines (1-based): 1,2 map to source 10; 3 maps to source 12; 4 has
        // no mapping (a label / directive that survived filtering).
        let mut map = HashMap::new();
        map.insert(1usize, 10u32);
        map.insert(2, 10);
        map.insert(3, 12);
        AsmView::new("mov eax, 1\nadd eax, 2\nret\n.Lfunc_end0:", map)
    }

    #[test]
    fn asm_to_source_lookup_is_one_based_to_zero_based() {
        let v = sample();
        assert_eq!(v.line_count(), 4);
        // asm row 0 (1-based 1) → source 10.
        assert_eq!(v.source_for_asm(0), Some(10));
        assert_eq!(v.source_for_asm(1), Some(10));
        assert_eq!(v.source_for_asm(2), Some(12));
        // asm row 3 has no mapping.
        assert_eq!(v.source_for_asm(3), None);
    }

    #[test]
    fn source_to_asm_reverse_index_sorted() {
        let v = sample();
        // source line 10 → asm rows 0 and 1 (0-based), sorted.
        assert_eq!(v.asm_rows_for_source(10), &[0, 1]);
        assert_eq!(v.asm_rows_for_source(12), &[2]);
        // A source line with no code.
        assert_eq!(v.asm_rows_for_source(99), &[] as &[usize]);
        // Scroll target is the first asm row.
        assert_eq!(v.first_asm_row_for_source(10), Some(0));
        assert_eq!(v.first_asm_row_for_source(99), None);
    }

    #[test]
    fn highlight_follows_selection() {
        let mut v = sample();
        assert!(!v.asm_row_highlighted(0)); // nothing selected yet
        v.select_source(10);
        assert!(v.asm_row_highlighted(0));
        assert!(v.asm_row_highlighted(1));
        assert!(!v.asm_row_highlighted(2)); // maps to source 12
        assert!(!v.asm_row_highlighted(3)); // unmapped
        v.select_source(12);
        assert!(v.asm_row_highlighted(2));
        assert!(!v.asm_row_highlighted(0));
    }

    #[test]
    fn empty_asm() {
        let v = AsmView::new("", HashMap::new());
        assert_eq!(v.line_count(), 0);
        assert_eq!(v.source_for_asm(0), None);
    }
}
