//! Cursor movement (`impl Buffer`). Every op takes `extend: bool` — when set,
//! the anchor is kept and only the head moves (shift-select); otherwise the
//! selection collapses to the new caret.
//!
//! ## Word navigation (⌥→ / ⌥←) — the punctuation-aware rule
//! Ported from Monaco's `cursorWordEndRight` / `cursorWordStartLeft`
//! (editor-manager.ts:106-119). Characters fall into three classes:
//!
//! * **Word**  — `[A-Za-z0-9_]` plus any Unicode alphanumeric.
//! * **Whitespace** — anything `char::is_whitespace` (includes `\n`).
//! * **Punct** — everything else (`. ( ) [ ] { } , ; : + - * / …`).
//!
//! `word_right` (⌥→, "word end right"): skip any leading whitespace, then consume
//! the maximal run of a single class and stop after it. `word_left` (⌥←, "word
//! start left"): the mirror — skip trailing whitespace leftward, then consume the
//! maximal run of one class. This makes the caret **stop at every punctuation
//! boundary** instead of jumping across the token.
//!
//! Worked example — `foo_bar.baz(qux)` (documented as the test fixture):
//! ⌥→ from col 0 stops at `7 . 8 11 ( 12 15 ) 16` →
//! `[7, 8, 11, 12, 15, 16]`; ⌥← from col 16 stops at
//! `[15, 12, 11, 8, 7, 0]` (symmetric).

use unicode_segmentation::GraphemeCursor;

use crate::buffer::Buffer;
use crate::selection::{Cursor, Selection};

/// Character class for word navigation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Whitespace,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

impl Buffer {
    // ---- primitive offset motions (byte offsets) ------------------------

    /// Byte offset of the grapheme boundary immediately after `offset`.
    pub(crate) fn grapheme_after(&self, offset: usize) -> usize {
        if offset >= self.len_bytes() {
            return self.len_bytes();
        }
        let row = self.byte_to_row(offset);
        let ls = self.line_start_byte(row);
        let local = offset - ls;
        let line = self.line(row);
        if local < line.len() {
            let mut gc = GraphemeCursor::new(local, line.len(), true);
            match gc.next_boundary(&line, 0) {
                Ok(Some(nb)) => ls + nb,
                _ => offset + 1,
            }
        } else {
            // At line end: step across the `\n` to the next line start.
            offset + 1
        }
    }

    /// Byte offset of the grapheme boundary immediately before `offset`.
    pub(crate) fn grapheme_before(&self, offset: usize) -> usize {
        if offset == 0 {
            return 0;
        }
        let row = self.byte_to_row(offset);
        let ls = self.line_start_byte(row);
        let local = offset - ls;
        if local > 0 {
            let line = self.line(row);
            let mut gc = GraphemeCursor::new(local, line.len(), true);
            match gc.prev_boundary(&line, 0) {
                Ok(Some(pb)) => ls + pb,
                _ => offset - 1,
            }
        } else {
            // At column 0: step back across the `\n` to the previous line end.
            offset - 1
        }
    }

    /// ⌥→ target from `offset`: end of the next word/token (punctuation-aware).
    pub(crate) fn word_right(&self, offset: usize) -> usize {
        let n = self.len_chars();
        let mut ci = self.byte_to_char(offset);
        while ci < n && self.char_at_ci(ci).is_whitespace() {
            ci += 1;
        }
        if ci >= n {
            return self.len_bytes();
        }
        let cls = class(self.char_at_ci(ci));
        while ci < n && class(self.char_at_ci(ci)) == cls {
            ci += 1;
        }
        self.char_to_byte(ci)
    }

    /// ⌥← target from `offset`: start of the previous word/token.
    pub(crate) fn word_left(&self, offset: usize) -> usize {
        let mut ci = self.byte_to_char(offset);
        while ci > 0 && self.char_at_ci(ci - 1).is_whitespace() {
            ci -= 1;
        }
        if ci == 0 {
            return 0;
        }
        let cls = class(self.char_at_ci(ci - 1));
        while ci > 0 && class(self.char_at_ci(ci - 1)) == cls {
            ci -= 1;
        }
        self.char_to_byte(ci)
    }

    // ---- rope char helpers (crate-internal) -----------------------------

    pub(crate) fn byte_to_char(&self, byte: usize) -> usize {
        self.rope_ref().byte_to_char(byte)
    }

    pub(crate) fn char_to_byte(&self, ch: usize) -> usize {
        self.rope_ref().char_to_byte(ch)
    }

    fn char_at_ci(&self, ci: usize) -> char {
        self.rope_ref().char(ci)
    }

    // ---- generic motion driver -----------------------------------------

    /// Recompute every cursor's head via `motion`, which returns
    /// `(new_head, new_goal_col)` from the current `(head, goal_col)`.
    fn move_all(
        &mut self,
        extend: bool,
        motion: impl Fn(&Buffer, usize, Option<usize>) -> (usize, Option<usize>),
    ) {
        let mut next = Vec::with_capacity(self.cursors.len());
        for c in &self.cursors.cursors {
            let (head, goal) = motion(self, c.sel.head, c.goal_col);
            let anchor = if extend { c.sel.anchor } else { head };
            next.push(Cursor {
                sel: Selection { anchor, head },
                goal_col: goal,
            });
        }
        self.cursors.cursors = next;
    }

    // ---- horizontal -----------------------------------------------------

    /// Move left one grapheme. With a non-empty selection and `!extend`, collapse
    /// to the selection start instead of stepping (standard editor feel).
    pub fn move_left(&mut self, extend: bool) {
        if !extend {
            self.collapse_horizontal(false);
            return;
        }
        self.move_all(true, |b, head, _| (b.grapheme_before(head), None));
    }

    /// Move right one grapheme. With a non-empty selection and `!extend`,
    /// collapse to the selection end.
    pub fn move_right(&mut self, extend: bool) {
        if !extend {
            self.collapse_horizontal(true);
            return;
        }
        self.move_all(true, |b, head, _| (b.grapheme_after(head), None));
    }

    /// Collapse each cursor: an empty selection steps one grapheme in `dir`
    /// (`true` = right); a non-empty selection collapses to its edge in `dir`.
    fn collapse_horizontal(&mut self, right: bool) {
        let mut next = Vec::with_capacity(self.cursors.len());
        for c in &self.cursors.cursors {
            let head = match (c.sel.is_empty(), right) {
                (true, true) => self.grapheme_after(c.sel.head),
                (true, false) => self.grapheme_before(c.sel.head),
                (false, true) => c.sel.end(),
                (false, false) => c.sel.start(),
            };
            next.push(Cursor {
                sel: Selection::at(head),
                goal_col: None,
            });
        }
        self.cursors.cursors = next;
    }

    /// ⌥← — move to the start of the previous word (punctuation-aware).
    pub fn move_word_left(&mut self, extend: bool) {
        self.move_all(extend, |b, head, _| (b.word_left(head), None));
    }

    /// ⌥→ — move to the end of the next word (punctuation-aware).
    pub fn move_word_right(&mut self, extend: bool) {
        self.move_all(extend, |b, head, _| (b.word_right(head), None));
    }

    // ---- vertical (sticky goal column) ----------------------------------

    /// Move up one line, preserving the sticky goal (char) column.
    pub fn move_up(&mut self, extend: bool) {
        self.move_all(extend, |b, head, goal| b.vertical(head, goal, -1));
    }

    /// Move down one line, preserving the sticky goal (char) column.
    pub fn move_down(&mut self, extend: bool) {
        self.move_all(extend, |b, head, goal| b.vertical(head, goal, 1));
    }

    /// Shared vertical step: `dir` is -1 (up) or +1 (down). Returns the new head
    /// and the goal column to carry forward.
    fn vertical(&self, head: usize, goal: Option<usize>, dir: isize) -> (usize, Option<usize>) {
        let point = self.offset_to_point(head);
        let want = goal.unwrap_or(point.col);
        let last = self.line_count().saturating_sub(1);
        let target_row = if dir < 0 {
            point.row.saturating_sub(1)
        } else {
            (point.row + 1).min(last)
        };
        let col = want.min(self.line_char_len(target_row));
        let line_start = self.byte_to_char(self.line_start_byte(target_row));
        (self.char_to_byte(line_start + col), Some(want))
    }

    // ---- line / document ends ------------------------------------------

    /// Move to column 0 of the current line. (Plain home — no first-non-
    /// whitespace toggle; the TS editor does not have one.)
    pub fn move_home(&mut self, extend: bool) {
        self.move_all(extend, |b, head, _| {
            let row = b.byte_to_row(head);
            (b.line_start_byte(row), None)
        });
    }

    /// Move to the end of the current line (before any trailing `\n`).
    pub fn move_end(&mut self, extend: bool) {
        self.move_all(extend, |b, head, _| {
            let row = b.byte_to_row(head);
            (b.line_end_byte(row), None)
        });
    }

    /// Move to the very start of the document.
    pub fn move_doc_start(&mut self, extend: bool) {
        self.move_all(extend, |_, _, _| (0, None));
    }

    /// Move to the very end of the document.
    pub fn move_doc_end(&mut self, extend: bool) {
        self.move_all(extend, |b, _, _| (b.len_bytes(), None));
    }

    /// Select the entire buffer (anchor 0, head end).
    pub fn select_all(&mut self) {
        self.set_selection(Selection::new(0, self.len_bytes()));
    }
}
