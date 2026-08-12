//! Find / replace matcher + input state (Ctrl+F / ⌘F).
//!
//! Pure logic, no GPUI: the find bar in `panels/code_view.rs` is a thin renderer
//! over this, and `JadeApp` owns a `Option<FindState>` (`Some` == bar open). Like
//! Quick Open the bar is a **captured-keystroke** buffer — but unlike Quick Open
//! the two text fields (find + replace) carry a real caret (`cursor`): printable
//! chars insert at it, Backspace/Delete remove around it, ←/→/Home/End move it,
//! and a click maps to a char column — so all of it is unit-tested without a
//! window. Enter navigates matches, Tab switches field.
//!
//! Matching is **byte-offset** so results map straight onto the buffer's byte
//! coordinates. Case-insensitivity uses ASCII case folding (`eq_ignore_ascii_case`),
//! which never changes a byte's length, so every match range stays on char
//! boundaries even in files with multi-byte UTF-8.

use std::ops::Range;

/// Which of the two input fields the captured keystrokes edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FindField {
    #[default]
    Find,
    Replace,
}

/// Transient find/replace state. Held as `Option<FindState>` on `JadeApp`.
#[derive(Debug, Clone, Default)]
pub struct FindState {
    /// The search needle.
    pub query: String,
    /// The replacement text (only meaningful when `replace_visible`).
    pub replace: String,
    /// Case-sensitive matching when true (default: insensitive).
    pub case_sensitive: bool,
    /// Whether the replace row is shown (⌘⌥F / the ⇄ toggle).
    pub replace_visible: bool,
    /// Which field captured keystrokes edit.
    pub field: FindField,
    /// Byte ranges of every match in the current buffer text, ascending.
    pub matches: Vec<Range<usize>>,
    /// Index into `matches` of the active match, if any.
    pub current: Option<usize>,
    /// Caret byte offset within the **active** field's string. Always on a char
    /// boundary; resets to end-of-text whenever the active field changes.
    pub cursor: usize,
}

/// Byte offset of the `col`-th char of `s` (clamped to the end).
pub fn byte_for_col(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map(|(i, _)| i).unwrap_or(s.len())
}

/// Char column of byte offset `byte` in `s` (`byte` must be a char boundary).
pub fn col_for_byte(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

/// All non-overlapping matches of `query` in `text`, as ascending byte ranges.
/// Empty when `query` is empty. Case-insensitive matching folds ASCII only
/// (`eq_ignore_ascii_case`), which preserves byte length so ranges land on char
/// boundaries.
pub fn find_matches(text: &str, query: &str, case_sensitive: bool) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let hay = text.as_bytes();
    let needle = query.as_bytes();
    let n = needle.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i + n <= hay.len() {
        let window = &hay[i..i + n];
        let hit = if case_sensitive {
            window == needle
        } else {
            window.eq_ignore_ascii_case(needle)
        };
        if hit {
            out.push(i..i + n);
            i += n; // non-overlapping
        } else {
            i += 1;
        }
    }
    out
}

impl FindState {
    /// A fresh bar with `replace` shown or not.
    pub fn new(replace_visible: bool) -> Self {
        FindState {
            replace_visible,
            ..Default::default()
        }
    }

    /// Recompute `matches` from `text`, keeping `current` on the same-or-nearest
    /// match: if the previously-current range still exists it stays selected,
    /// otherwise the selection clamps into range (or clears when empty).
    pub fn recompute(&mut self, text: &str) {
        let prev = self.current_range();
        self.matches = find_matches(text, &self.query, self.case_sensitive);
        self.current = match prev {
            _ if self.matches.is_empty() => None,
            Some(r) => Some(
                self.matches
                    .iter()
                    .position(|m| m.start >= r.start)
                    .unwrap_or(0),
            ),
            None => None,
        };
    }

    /// Point `current` at the first match starting at/after `caret` (wrapping to
    /// the first match), so opening the bar selects the match nearest the cursor.
    pub fn select_from(&mut self, caret: usize) {
        if self.matches.is_empty() {
            self.current = None;
            return;
        }
        self.current = Some(
            self.matches
                .iter()
                .position(|m| m.start >= caret)
                .unwrap_or(0),
        );
    }

    /// The active match's byte range, if any.
    pub fn current_range(&self) -> Option<Range<usize>> {
        self.current.and_then(|i| self.matches.get(i).cloned())
    }

    /// Advance to the next match (wrapping), returning its range.
    pub fn next(&mut self) -> Option<Range<usize>> {
        if self.matches.is_empty() {
            self.current = None;
            return None;
        }
        self.current = Some(match self.current {
            Some(i) => (i + 1) % self.matches.len(),
            None => 0,
        });
        self.current_range()
    }

    /// Step to the previous match (wrapping), returning its range.
    pub fn prev(&mut self) -> Option<Range<usize>> {
        if self.matches.is_empty() {
            self.current = None;
            return None;
        }
        self.current = Some(match self.current {
            Some(0) | None => self.matches.len() - 1,
            Some(i) => i - 1,
        });
        self.current_range()
    }

    /// Total matches (the `N` in `k of N`).
    pub fn count(&self) -> usize {
        self.matches.len()
    }

    /// 1-based ordinal of the active match (the `k` in `k of N`), or 0 when none.
    pub fn ordinal(&self) -> usize {
        self.current.map(|i| i + 1).unwrap_or(0)
    }

    /// The active field's text (the one the caret lives in).
    pub fn active_field(&self) -> &str {
        match self.field {
            FindField::Find => &self.query,
            FindField::Replace => &self.replace,
        }
    }

    /// A mutable handle to the field the keystrokes edit.
    fn active_field_mut(&mut self) -> &mut String {
        match self.field {
            FindField::Find => &mut self.query,
            FindField::Replace => &mut self.replace,
        }
    }

    /// Insert a printable string at the caret. Returns true when the find query
    /// changed (so the caller re-runs the match scan).
    pub fn type_str(&mut self, s: &str) -> bool {
        let editing_query = self.field == FindField::Find;
        let at = self.cursor.min(self.active_field().len());
        self.active_field_mut().insert_str(at, s);
        self.cursor = at + s.len();
        editing_query
    }

    /// Delete the char before the caret. Returns true when the query changed.
    pub fn backspace(&mut self) -> bool {
        let editing_query = self.field == FindField::Find;
        let at = self.cursor.min(self.active_field().len());
        let Some((start, _)) = self.active_field()[..at].char_indices().next_back() else {
            return false; // caret at the front — nothing to delete
        };
        self.active_field_mut().replace_range(start..at, "");
        self.cursor = start;
        editing_query
    }

    /// Delete the char after the caret (forward delete). Returns true when the
    /// query changed.
    pub fn delete_forward(&mut self) -> bool {
        let editing_query = self.field == FindField::Find;
        let at = self.cursor.min(self.active_field().len());
        let Some(c) = self.active_field()[at..].chars().next() else {
            return false; // caret at the end — nothing to delete
        };
        let end = at + c.len_utf8();
        self.active_field_mut().replace_range(at..end, "");
        self.cursor = at;
        editing_query
    }

    /// Move the caret one char left.
    pub fn move_left(&mut self) {
        let at = self.cursor.min(self.active_field().len());
        self.cursor = self.active_field()[..at]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    /// Move the caret one char right.
    pub fn move_right(&mut self) {
        let at = self.cursor.min(self.active_field().len());
        self.cursor = match self.active_field()[at..].chars().next() {
            Some(c) => at + c.len_utf8(),
            None => at,
        };
    }

    /// Jump the caret to the front of the field.
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Jump the caret to the end of the field.
    pub fn move_end(&mut self) {
        self.cursor = self.active_field().len();
    }

    /// Put the caret at char column `col` of the active field (a click).
    pub fn set_cursor_col(&mut self, col: usize) {
        self.cursor = byte_for_col(self.active_field(), col);
    }

    /// Make `field` active (a click into it), keeping the caret in range.
    pub fn focus_field(&mut self, field: FindField) {
        if self.field != field {
            self.field = field;
            self.move_end();
        }
        if field == FindField::Replace {
            self.replace_visible = true;
        }
    }

    /// Toggle the replace row on; when turning it on, move the caret to it.
    pub fn show_replace(&mut self) {
        self.replace_visible = true;
        self.field = FindField::Replace;
        self.move_end();
    }

    /// Tab / ⇧Tab between the find and (visible) replace fields.
    pub fn switch_field(&mut self) {
        if !self.replace_visible {
            return;
        }
        self.field = match self.field {
            FindField::Find => FindField::Replace,
            FindField::Replace => FindField::Find,
        };
        self.move_end();
    }

    /// Flip case sensitivity (the caller re-runs `recompute` after).
    pub fn toggle_case(&mut self) {
        self.case_sensitive = !self.case_sensitive;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_case_insensitive_by_default() {
        let m = find_matches("Foo foo FOO", "foo", false);
        assert_eq!(m, vec![0..3, 4..7, 8..11]);
    }

    #[test]
    fn matches_case_sensitive() {
        let m = find_matches("Foo foo FOO", "foo", true);
        assert_eq!(m, vec![4..7]);
    }

    #[test]
    fn matches_non_overlapping() {
        // "aa" in "aaaa" → two matches, not three.
        assert_eq!(find_matches("aaaa", "aa", true), vec![0..2, 2..4]);
    }

    #[test]
    fn empty_query_no_matches() {
        assert!(find_matches("anything", "", false).is_empty());
    }

    #[test]
    fn matches_land_on_char_boundaries_in_utf8() {
        // "éé" — each é is 2 bytes. Searching "é" yields byte ranges 0..2, 2..4.
        let text = "éé";
        let m = find_matches(text, "é", false);
        assert_eq!(m, vec![0..2, 2..4]);
        // Ranges are valid char boundaries (slicing would panic otherwise).
        for r in &m {
            assert_eq!(&text[r.clone()], "é");
        }
    }

    #[test]
    fn select_from_caret_picks_nearest_forward() {
        let mut s = FindState::new(false);
        s.query = "x".into();
        s.recompute("x..x..x"); // matches at 0, 3, 6
        s.select_from(1);
        assert_eq!(s.current, Some(1)); // the match at byte 3
        s.select_from(7);
        assert_eq!(s.current, Some(0)); // wraps to the first
    }

    #[test]
    fn next_prev_wrap() {
        let mut s = FindState::new(false);
        s.query = "x".into();
        s.recompute("x.x.x"); // 0, 2, 4
        s.current = Some(0);
        assert_eq!(s.next(), Some(2..3));
        assert_eq!(s.next(), Some(4..5));
        assert_eq!(s.next(), Some(0..1)); // wrap
        assert_eq!(s.prev(), Some(4..5)); // wrap back
    }

    #[test]
    fn recompute_keeps_selection_near_previous() {
        let mut s = FindState::new(false);
        s.query = "x".into();
        s.recompute("x.x.x");
        s.current = Some(2); // byte 4
        // Same text: the match at byte 4 stays selected.
        s.recompute("x.x.x");
        assert_eq!(s.current, Some(2));
    }

    #[test]
    fn typing_into_fields() {
        let mut s = FindState::new(true);
        assert!(s.type_str("ab")); // find field → query changed
        assert_eq!(s.query, "ab");
        s.switch_field();
        assert_eq!(s.field, FindField::Replace);
        assert!(!s.type_str("cd")); // replace field → query unchanged
        assert_eq!(s.replace, "cd");
        assert!(!s.backspace());
        assert_eq!(s.replace, "c");
    }

    #[test]
    fn caret_inserts_and_deletes_mid_text() {
        let mut s = FindState::new(false);
        s.type_str("held");
        s.move_left();
        s.move_left(); // he|ld
        assert!(s.type_str("l")); // hel|ld
        assert_eq!(s.query, "helld");
        assert!(s.backspace()); // he|ld
        assert_eq!(s.query, "held");
        assert!(s.delete_forward()); // he|d
        assert_eq!(s.query, "hed");
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn caret_bounds_and_jumps() {
        let mut s = FindState::new(false);
        s.type_str("ab");
        s.move_home();
        assert!(!s.backspace(), "backspace at front deletes nothing");
        assert_eq!(s.query, "ab");
        s.move_left();
        assert_eq!(s.cursor, 0, "left at front stays put");
        s.move_end();
        assert!(!s.delete_forward(), "forward delete at end deletes nothing");
        s.move_right();
        assert_eq!(s.cursor, 2, "right at end stays put");
    }

    #[test]
    fn caret_moves_by_chars_in_utf8() {
        let mut s = FindState::new(false);
        s.type_str("éx"); // é is 2 bytes
        s.move_home();
        s.move_right();
        assert_eq!(s.cursor, 2, "caret lands after the 2-byte é");
        s.backspace();
        assert_eq!(s.query, "x");
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn click_column_maps_to_bytes() {
        assert_eq!(byte_for_col("abc", 0), 0);
        assert_eq!(byte_for_col("abc", 2), 2);
        assert_eq!(byte_for_col("abc", 9), 3); // clamps to end
        assert_eq!(byte_for_col("éxé", 1), 2); // past the 2-byte é
        assert_eq!(col_for_byte("éxé", 2), 1);

        let mut s = FindState::new(false);
        s.type_str("abc");
        s.set_cursor_col(1);
        s.type_str("Z");
        assert_eq!(s.query, "aZbc");
    }

    #[test]
    fn field_switches_reset_the_caret() {
        let mut s = FindState::new(true);
        s.type_str("abc");
        s.move_home();
        s.switch_field(); // → replace
        s.type_str("xy");
        s.focus_field(FindField::Find);
        assert_eq!(s.cursor, 3, "caret lands at the end of the find text");
        s.set_cursor_col(0);
        s.focus_field(FindField::Find);
        assert_eq!(s.cursor, 0, "re-clicking the same field keeps the caret");
    }

    #[test]
    fn ordinal_and_count() {
        let mut s = FindState::new(false);
        s.query = "x".into();
        s.recompute("x.x.x");
        s.current = Some(1);
        assert_eq!(s.ordinal(), 2);
        assert_eq!(s.count(), 3);
        s.current = None;
        assert_eq!(s.ordinal(), 0);
    }
}
