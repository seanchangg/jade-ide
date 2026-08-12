//! Typing & editing helpers (`impl Buffer`) that reproduce the Electron editor's
//! feel (§4.1): bracket-pair auto-close with type-over, auto-surround, Enter
//! auto-indent, and tab-as-spaces. Table-driven via [`PAIRS`] so the behavior is
//! auditable and unit-tested.

use crate::buffer::{Buffer, PlannedEdit};
use crate::edit::EditRecord;
use crate::selection::Selection;

/// Auto-close / surround pairs, `(open, close)`. Quotes are self-closing (open ==
/// close). Metal `/*`→`*/` comment continuation is intentionally out of scope.
pub const PAIRS: &[(char, char)] = &[
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('"', '"'),
    ('\'', '\''),
];

/// Spaces per tab stop (§4.1: tabSize 4, insert-spaces).
pub const TAB_WIDTH: usize = 4;

/// If `ch` opens a pair, its closing char.
fn closer_for(ch: char) -> Option<char> {
    PAIRS.iter().find(|(o, _)| *o == ch).map(|(_, c)| *c)
}

/// True if `ch` is a closing char of some pair (or a quote, which is both).
fn is_closer(ch: char) -> bool {
    PAIRS.iter().any(|(_, c)| *c == ch)
}

impl Buffer {
    /// Type a single character with bracket/quote smarts. Handles, per cursor:
    /// auto-surround (non-empty selection + opener), typing-over a selection,
    /// bracket type-over (closing char with the same char to the right → step
    /// over), auto-close (opener → insert the pair, caret between), and plain
    /// insertion. Coalesces into the current undo group.
    pub fn type_char(&mut self, ch: char) -> EditRecord {
        let mut plans: Vec<PlannedEdit> = Vec::new();
        let mut finals: Vec<Selection> = Vec::new();
        // Running byte delta so multi-cursor final positions stay correct.
        let mut delta: i64 = 0;
        let ch_len = ch.len_utf8();

        let sels = self.cursors.selections();
        for sel in sels {
            let shift = |off: usize| (off as i64 + delta) as usize;
            if !sel.is_empty() {
                let (start, end) = (sel.start(), sel.end());
                if let Some(close) = closer_for(ch) {
                    // Auto-surround: wrap the selection, keep it around the inner
                    // text.
                    let inner_len = end - start;
                    plans.push(PlannedEdit {
                        start,
                        end: start,
                        text: ch.to_string(),
                    });
                    plans.push(PlannedEdit {
                        start: end,
                        end,
                        text: close.to_string(),
                    });
                    let a = shift(start) + ch_len;
                    finals.push(Selection::new(a, a + inner_len));
                    delta += ch_len as i64 + close.len_utf8() as i64;
                } else {
                    // Type over the selection.
                    plans.push(PlannedEdit {
                        start,
                        end,
                        text: ch.to_string(),
                    });
                    finals.push(Selection::at(shift(start) + ch_len));
                    delta += ch_len as i64 - (end - start) as i64;
                }
                continue;
            }

            let caret = sel.head;
            let right = self.char_at(caret);
            if is_closer(ch) && right == Some(ch) {
                // Type-over: step across the existing closing char. No text edit.
                finals.push(Selection::at(shift(caret) + ch_len));
            } else if self.should_auto_close(ch, caret) {
                let close = closer_for(ch).unwrap();
                plans.push(PlannedEdit {
                    start: caret,
                    end: caret,
                    text: format!("{ch}{close}"),
                });
                finals.push(Selection::at(shift(caret) + ch_len));
                delta += ch_len as i64 + close.len_utf8() as i64;
            } else {
                plans.push(PlannedEdit {
                    start: caret,
                    end: caret,
                    text: ch.to_string(),
                });
                finals.push(Selection::at(shift(caret) + ch_len));
                delta += ch_len as i64;
            }
        }
        self.transact(plans, finals, true)
    }

    /// Whether typing `ch` at `caret` should auto-close. Brackets always do;
    /// quotes only when not adjacent to a word char on either side (so an
    /// apostrophe in `don't` or a closing string quote isn't doubled).
    fn should_auto_close(&self, ch: char, caret: usize) -> bool {
        let Some(close) = closer_for(ch) else {
            return false;
        };
        if ch != close {
            return true; // a real bracket
        }
        // Quote: suppress when a word char sits immediately left or right.
        let left_word = self.char_before(caret).is_some_and(is_word);
        let right_word = self.char_at(caret).is_some_and(is_word);
        !left_word && !right_word
    }

    /// The char immediately before `offset`, if any.
    fn char_before(&self, offset: usize) -> Option<char> {
        if offset == 0 {
            None
        } else {
            let b = self.grapheme_before(offset);
            self.char_at(b)
        }
    }

    /// Insert a newline at each caret, copying the current line's leading
    /// whitespace (auto-indent). A non-empty selection is replaced.
    pub fn insert_newline(&mut self) -> EditRecord {
        let mut plans = Vec::new();
        let mut finals = Vec::new();
        let mut delta: i64 = 0;
        for sel in self.cursors.selections() {
            let (start, end) = (sel.start(), sel.end());
            let row = self.byte_to_row(start);
            let indent = self.leading_whitespace(row);
            let text = format!("\n{indent}");
            let tlen = text.len();
            plans.push(PlannedEdit {
                start,
                end,
                text,
            });
            let caret = (start as i64 + delta) as usize + tlen;
            finals.push(Selection::at(caret));
            delta += tlen as i64 - (end - start) as i64;
        }
        self.transact(plans, finals, true)
    }

    /// Insert spaces to the next tab stop at each caret (insert-spaces mode). A
    /// non-empty selection is replaced by the spaces.
    pub fn insert_tab(&mut self) -> EditRecord {
        let mut plans = Vec::new();
        let mut finals = Vec::new();
        let mut delta: i64 = 0;
        for sel in self.cursors.selections() {
            let (start, end) = (sel.start(), sel.end());
            let col = self.offset_to_point(start).col;
            let n = TAB_WIDTH - (col % TAB_WIDTH);
            let text = " ".repeat(n);
            plans.push(PlannedEdit {
                start,
                end,
                text,
            });
            let caret = (start as i64 + delta) as usize + n;
            finals.push(Selection::at(caret));
            delta += n as i64 - (end - start) as i64;
        }
        self.transact(plans, finals, true)
    }

    /// Insert arbitrary text at each caret / over each selection (e.g. paste).
    /// Standalone undo group.
    pub fn insert_text(&mut self, text: &str) -> EditRecord {
        let mut plans = Vec::new();
        let mut finals = Vec::new();
        let mut delta: i64 = 0;
        for sel in self.cursors.selections() {
            let (start, end) = (sel.start(), sel.end());
            plans.push(PlannedEdit {
                start,
                end,
                text: text.to_string(),
            });
            let caret = (start as i64 + delta) as usize + text.len();
            finals.push(Selection::at(caret));
            delta += text.len() as i64 - (end - start) as i64;
        }
        self.transact(plans, finals, false)
    }

    /// Delete backward: a non-empty selection, else one grapheme before the
    /// caret (grapheme-aware — an emoji deletes as one unit).
    pub fn delete_backward(&mut self) -> EditRecord {
        self.delete_with(|b, sel| {
            if sel.is_empty() {
                b.grapheme_before(sel.head)..sel.head
            } else {
                sel.range()
            }
        })
    }

    /// Delete forward: a non-empty selection, else one grapheme after the caret.
    pub fn delete_forward(&mut self) -> EditRecord {
        self.delete_with(|b, sel| {
            if sel.is_empty() {
                sel.head..b.grapheme_after(sel.head)
            } else {
                sel.range()
            }
        })
    }

    /// Delete the word before the caret (⌥⌫): a non-empty selection, else from
    /// the previous word start to the caret.
    pub fn delete_word_back(&mut self) -> EditRecord {
        self.delete_with(|b, sel| {
            if sel.is_empty() {
                b.word_left(sel.head)..sel.head
            } else {
                sel.range()
            }
        })
    }

    /// Shared deletion driver: `range_for` yields the byte range to remove for
    /// each selection. Carets collapse to each removed range's start.
    fn delete_with(
        &mut self,
        range_for: impl Fn(&Buffer, Selection) -> std::ops::Range<usize>,
    ) -> EditRecord {
        let mut plans = Vec::new();
        let mut finals = Vec::new();
        let mut delta: i64 = 0;
        for sel in self.cursors.selections() {
            let range = range_for(self, sel);
            let caret = (range.start as i64 + delta) as usize;
            finals.push(Selection::at(caret));
            if range.start != range.end {
                delta -= (range.end - range.start) as i64;
                plans.push(PlannedEdit {
                    start: range.start,
                    end: range.end,
                    text: String::new(),
                });
            }
        }
        self.transact(plans, finals, true)
    }

    /// Leading run of spaces/tabs on line `row`.
    fn leading_whitespace(&self, row: usize) -> String {
        self.line(row)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
