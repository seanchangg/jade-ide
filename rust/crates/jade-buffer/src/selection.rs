//! Selection / cursor model. A [`Selection`] is a pair of **byte offsets**
//! (`anchor`, `head`); a collapsed selection (`anchor == head`) is a plain
//! caret. [`CursorSet`] holds one-or-more cursors — the data structure is
//! multi-selection-ready, but the shipping editor drives a single selection.
//!
//! Byte offsets (not Points) are the cursor currency so that edits can shift
//! cursors with cheap integer math and so the model never desyncs from the rope.

use std::ops::Range;

/// A directed selection: `anchor` is the fixed end, `head` is the moving end
/// (where the caret visually sits). Both are byte offsets into the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    /// A collapsed caret at `offset`.
    pub fn at(offset: usize) -> Self {
        Selection {
            anchor: offset,
            head: offset,
        }
    }

    pub fn new(anchor: usize, head: usize) -> Self {
        Selection { anchor, head }
    }

    /// The caret (moving end).
    pub fn caret(&self) -> usize {
        self.head
    }

    /// No text is selected.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Lower byte offset of the selection.
    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    /// Upper byte offset of the selection.
    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    /// The selected byte range `[start, end)`.
    pub fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }
}

/// One cursor: a [`Selection`] plus its sticky goal column (char column the
/// caret "wants" to be at across vertical moves; `None` once a horizontal move
/// resets it).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursor {
    pub sel: Selection,
    pub goal_col: Option<usize>,
}

impl Cursor {
    pub(crate) fn at(offset: usize) -> Self {
        Cursor {
            sel: Selection::at(offset),
            goal_col: None,
        }
    }

    pub(crate) fn from_sel(sel: Selection) -> Self {
        Cursor {
            sel,
            goal_col: None,
        }
    }
}

/// The set of live cursors. Always holds at least one cursor; index 0 is the
/// primary. Multi-selection-ready (edits and motions fan out over every
/// cursor), but single-selection is the shipping behavior.
#[derive(Clone, Debug)]
pub struct CursorSet {
    pub(crate) cursors: Vec<Cursor>,
}

impl CursorSet {
    /// A single collapsed caret at `offset`.
    pub fn single(offset: usize) -> Self {
        CursorSet {
            cursors: vec![Cursor::at(offset)],
        }
    }

    /// The primary selection (index 0).
    pub fn primary(&self) -> Selection {
        self.cursors[0].sel
    }

    /// All selections, in cursor order.
    pub fn selections(&self) -> Vec<Selection> {
        self.cursors.iter().map(|c| c.sel).collect()
    }

    /// Number of cursors.
    pub fn len(&self) -> usize {
        self.cursors.len()
    }

    /// Always false (a `CursorSet` always has a primary), provided to satisfy
    /// clippy's `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        self.cursors.is_empty()
    }

    /// Replace all cursors with a single collapsed caret.
    pub(crate) fn set_single(&mut self, offset: usize) {
        self.cursors.clear();
        self.cursors.push(Cursor::at(offset));
    }

    /// Replace all cursors from a list of selections (goal columns cleared).
    pub(crate) fn set_from_selections(&mut self, sels: &[Selection]) {
        self.cursors.clear();
        if sels.is_empty() {
            self.cursors.push(Cursor::at(0));
        } else {
            self.cursors
                .extend(sels.iter().map(|s| Cursor::from_sel(*s)));
        }
    }
}

impl Default for CursorSet {
    fn default() -> Self {
        CursorSet::single(0)
    }
}
