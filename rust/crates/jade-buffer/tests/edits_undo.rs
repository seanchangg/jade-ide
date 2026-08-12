//! Edits, LSP-shaped records, batch atomicity, undo/redo grouping determinism,
//! and dirty tracking.

use jade_buffer::{Buffer, LspPosition, ManualClock, Selection};

fn manual() -> (Buffer, jade_buffer::ManualClockHandle) {
    let clock = ManualClock::new();
    let handle = clock.handle();
    (Buffer::with_clock("", Box::new(clock)), handle)
}

fn manual_text(text: &str) -> (Buffer, jade_buffer::ManualClockHandle) {
    let clock = ManualClock::new();
    let handle = clock.handle();
    (Buffer::with_clock(text, Box::new(clock)), handle)
}

#[test]
fn edit_replaces_range_and_moves_caret() {
    let mut b = Buffer::from_text("hello world");
    let rec = b.edit(0..5, "goodbye");
    assert_eq!(b.to_string(), "goodbye world");
    assert_eq!(b.selection().caret(), 7);
    assert_eq!(rec.version, 1);
}

#[test]
fn edit_record_carries_lsp_change() {
    let mut b = Buffer::from_text("hello");
    let rec = b.edit(1..3, "X");
    assert_eq!(rec.changes.len(), 1);
    let c = &rec.changes[0];
    assert_eq!(c.start, LspPosition::new(0, 1));
    assert_eq!(c.end, LspPosition::new(0, 3));
    assert_eq!(c.new_text, "X");
}

#[test]
fn lsp_change_positions_are_pre_edit_and_utf16() {
    // Insert after the emoji; the range must be in pre-edit UTF-16 coordinates.
    let mut b = Buffer::from_text("a🎉b");
    let rec = b.edit(5..5, "Z"); // byte 5 = right after 🎉
    assert_eq!(rec.changes[0].start, LspPosition::new(0, 3)); // a(1)+🎉(2)=3
    assert_eq!(b.to_string(), "a🎉Zb");
}

#[test]
fn batch_edit_is_atomic_single_undo() {
    let mut b = Buffer::from_text("aXbXc");
    // Replace both X's in one group.
    let rec = b.batch_edit(vec![(1..2, "-".into()), (3..4, "-".into())]);
    assert_eq!(b.to_string(), "a-b-c");
    assert_eq!(rec.changes.len(), 2);
    // One undo reverts the whole batch.
    assert!(b.undo());
    assert_eq!(b.to_string(), "aXbXc");
}

#[test]
fn batch_edit_lsp_changes_descend_by_position() {
    let mut b = Buffer::from_text("aXbXc");
    let rec = b.batch_edit(vec![(1..2, "-".into()), (3..4, "-".into())]);
    // Ordered descending so sequential application stays valid.
    assert_eq!(rec.changes[0].start, LspPosition::new(0, 3));
    assert_eq!(rec.changes[1].start, LspPosition::new(0, 1));
}

#[test]
fn batch_edit_applies_left_edits_without_disturbing_right() {
    // Different-length replacements: offsets must not corrupt each other.
    let mut b = Buffer::from_text("[1][2]");
    b.batch_edit(vec![(1..2, "one".into()), (4..5, "two".into())]);
    assert_eq!(b.to_string(), "[one][two]");
}

#[test]
fn undo_redo_restores_text() {
    let mut b = Buffer::from_text("abc");
    b.edit(3..3, "def");
    assert_eq!(b.to_string(), "abcdef");
    assert!(b.undo());
    assert_eq!(b.to_string(), "abc");
    assert!(b.redo());
    assert_eq!(b.to_string(), "abcdef");
}

#[test]
fn undo_restores_cursor_positions() {
    let mut b = Buffer::from_text("abc");
    b.set_caret(3);
    b.edit(3..3, "XY");
    assert_eq!(b.selection().caret(), 5);
    b.undo();
    // Caret returns to where it was before the edit.
    assert_eq!(b.selection().caret(), 3);
}

#[test]
fn undo_on_empty_history_returns_false() {
    let mut b = Buffer::from_text("abc");
    assert!(!b.undo());
    assert!(!b.redo());
}

#[test]
fn typing_within_window_coalesces_into_one_group() {
    let (mut b, clock) = manual();
    clock.set(0);
    b.type_char('a');
    clock.set(100);
    b.type_char('b');
    clock.set(250);
    b.type_char('c');
    assert_eq!(b.to_string(), "abc");
    // Single coalesced group → one undo clears all three.
    assert!(b.undo());
    assert_eq!(b.to_string(), "");
}

#[test]
fn typing_past_window_splits_groups() {
    let (mut b, clock) = manual();
    clock.set(0);
    b.type_char('a');
    clock.set(400); // > 300ms COALESCE window
    b.type_char('b');
    assert_eq!(b.to_string(), "ab");
    assert!(b.undo());
    assert_eq!(b.to_string(), "a"); // only 'b' undone
    assert!(b.undo());
    assert_eq!(b.to_string(), "");
}

#[test]
fn group_boundary_forces_split_within_window() {
    let (mut b, clock) = manual();
    clock.set(0);
    b.type_char('a');
    b.group_boundary();
    clock.set(50); // within window, but boundary forces a new group
    b.type_char('b');
    assert!(b.undo());
    assert_eq!(b.to_string(), "a");
}

#[test]
fn redo_stack_cleared_by_new_edit() {
    let (mut b, clock) = manual();
    clock.set(0);
    b.type_char('a');
    clock.set(400);
    b.type_char('b');
    b.undo(); // "a", 'b' on redo stack
    clock.set(800);
    b.type_char('c'); // new edit clears redo
    assert_eq!(b.to_string(), "ac");
    assert!(!b.redo());
}

#[test]
fn undo_breaks_coalescing() {
    let (mut b, clock) = manual_text("x");
    clock.set(0);
    b.set_caret(1);
    b.type_char('a');
    b.undo();
    clock.set(10); // within window of the undone edit's time, but undo reset it
    b.type_char('b');
    // The 'b' is its own group; undo removes just 'b'.
    b.undo();
    assert_eq!(b.to_string(), "x");
}

#[test]
fn dirty_tracking() {
    let mut b = Buffer::from_text("abc");
    assert!(!b.is_dirty());
    b.edit(0..0, "x");
    assert!(b.is_dirty());
    b.mark_saved();
    assert!(!b.is_dirty());
    b.edit(0..0, "y");
    assert!(b.is_dirty());
}

#[test]
fn version_increments_per_text_change() {
    let mut b = Buffer::from_text("");
    assert_eq!(b.version(), 0);
    b.edit(0..0, "a");
    assert_eq!(b.version(), 1);
    b.edit(1..1, "b");
    assert_eq!(b.version(), 2);
    b.undo();
    assert_eq!(b.version(), 3); // undo is a versioned change too
}

#[test]
fn set_selection_and_edit_over_selection() {
    let mut b = Buffer::from_text("hello");
    b.set_selection(Selection::new(0, 5));
    b.insert_text("bye");
    assert_eq!(b.to_string(), "bye");
    assert_eq!(b.selection().caret(), 3);
}
