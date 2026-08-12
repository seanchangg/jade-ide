//! Cursor movement: grapheme arrows, punctuation-aware word nav (Monaco
//! fixtures), sticky goal column, home/end, doc start/end, and selection
//! (extend) variants.

use jade_buffer::{Buffer, Selection};

/// Collect successive ⌥→ caret stops from `start` until end-of-buffer.
fn word_right_stops(b: &mut Buffer, start: usize) -> Vec<usize> {
    b.set_caret(start);
    let mut stops = Vec::new();
    loop {
        let before = b.selection().caret();
        b.move_word_right(false);
        let now = b.selection().caret();
        if now == before {
            break;
        }
        stops.push(now);
    }
    stops
}

/// Collect successive ⌥← caret stops from `start` until offset 0.
fn word_left_stops(b: &mut Buffer, start: usize) -> Vec<usize> {
    b.set_caret(start);
    let mut stops = Vec::new();
    loop {
        let before = b.selection().caret();
        b.move_word_left(false);
        let now = b.selection().caret();
        if now == before {
            break;
        }
        stops.push(now);
    }
    stops
}

#[test]
fn word_right_stops_at_punctuation_boundaries() {
    // foo_bar.baz(qux) — the documented Monaco fixture.
    // indices: f0 o1 o2 _3 b4 a5 r6 .7 b8 a9 z10 (11 q12 u13 x14 )15
    let mut b = Buffer::from_text("foo_bar.baz(qux)");
    assert_eq!(word_right_stops(&mut b, 0), vec![7, 8, 11, 12, 15, 16]);
}

#[test]
fn word_left_stops_are_symmetric() {
    let mut b = Buffer::from_text("foo_bar.baz(qux)");
    assert_eq!(word_left_stops(&mut b, 16), vec![15, 12, 11, 8, 7, 0]);
}

#[test]
fn word_right_skips_leading_whitespace_onto_next_token() {
    // "  ab  cd": stops at end of ab (4), then end of cd (8).
    let mut b = Buffer::from_text("  ab  cd");
    assert_eq!(word_right_stops(&mut b, 0), vec![4, 8]);
}

#[test]
fn word_right_treats_run_of_punctuation_as_one_token() {
    // "a==b": a(0) then "==" run (1..3) then b.
    let mut b = Buffer::from_text("a==b");
    assert_eq!(word_right_stops(&mut b, 0), vec![1, 3, 4]);
}

#[test]
fn word_nav_crosses_lines_as_whitespace() {
    // Newlines are whitespace; word-right from end of line 0 lands on line 1's
    // token end.
    let mut b = Buffer::from_text("foo\nbar");
    b.set_caret(3); // end of "foo"
    b.move_word_right(false);
    assert_eq!(b.selection().caret(), 7); // end of "bar"
}

#[test]
fn move_right_and_left_by_grapheme_over_emoji() {
    // "a🎉b": a@0, 🎉@1..5, b@5, len 6.
    let mut b = Buffer::from_text("a🎉b");
    b.set_caret(0);
    b.move_right(false);
    assert_eq!(b.selection().caret(), 1);
    b.move_right(false);
    assert_eq!(b.selection().caret(), 5); // emoji skipped as one grapheme
    b.move_left(false);
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn move_left_at_start_and_right_at_end_are_clamped() {
    let mut b = Buffer::from_text("ab");
    b.set_caret(0);
    b.move_left(false);
    assert_eq!(b.selection().caret(), 0);
    b.set_caret(2);
    b.move_right(false);
    assert_eq!(b.selection().caret(), 2);
}

#[test]
fn move_left_crosses_line_boundary() {
    let mut b = Buffer::from_text("ab\ncd");
    b.set_caret(3); // col 0 of line 1
    b.move_left(false);
    assert_eq!(b.selection().caret(), 2); // end of line 0
}

#[test]
fn move_right_collapses_selection_to_end() {
    let mut b = Buffer::from_text("hello");
    b.set_selection(Selection::new(1, 3));
    b.move_right(false);
    assert_eq!(b.selection().caret(), 3);
    assert!(b.selection().is_empty());
}

#[test]
fn move_left_collapses_selection_to_start() {
    let mut b = Buffer::from_text("hello");
    b.set_selection(Selection::new(1, 3));
    b.move_left(false);
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn vertical_move_keeps_sticky_goal_column() {
    // Long, short, long lines. Goal column 5 survives the short middle line.
    let mut b = Buffer::from_text("abcdef\nab\nabcdef");
    b.set_caret(5); // row 0, col 5
    b.move_down(false);
    assert_eq!(b.offset_to_point(b.selection().caret()).col, 2); // clamped
    b.move_down(false);
    assert_eq!(b.offset_to_point(b.selection().caret()).col, 5); // restored
}

#[test]
fn move_up_at_top_stays_on_first_line() {
    let mut b = Buffer::from_text("abc\ndef");
    b.set_caret(1);
    b.move_up(false);
    let p = b.offset_to_point(b.selection().caret());
    assert_eq!(p.row, 0);
}

#[test]
fn home_goes_to_column_zero_not_first_nonwhitespace() {
    let mut b = Buffer::from_text("    foo");
    b.set_caret(7);
    b.move_home(false);
    assert_eq!(b.selection().caret(), 0); // plain column 0
}

#[test]
fn end_goes_to_line_end_before_newline() {
    let mut b = Buffer::from_text("foo\nbar");
    b.set_caret(0);
    b.move_end(false);
    assert_eq!(b.selection().caret(), 3);
}

#[test]
fn doc_start_and_end() {
    let mut b = Buffer::from_text("foo\nbar\nbaz");
    b.set_caret(5);
    b.move_doc_start(false);
    assert_eq!(b.selection().caret(), 0);
    b.move_doc_end(false);
    assert_eq!(b.selection().caret(), b.len_bytes());
}

#[test]
fn extend_variants_keep_anchor() {
    let mut b = Buffer::from_text("hello world");
    b.set_caret(0);
    b.move_word_right(true); // ⌥⇧→
    let sel = b.selection();
    assert_eq!(sel.anchor, 0);
    assert_eq!(sel.head, 5); // end of "hello"
    assert!(!sel.is_empty());
}

#[test]
fn extend_right_grows_selection() {
    let mut b = Buffer::from_text("abcd");
    b.set_caret(1);
    b.move_right(true);
    b.move_right(true);
    let sel = b.selection();
    assert_eq!((sel.anchor, sel.head), (1, 3));
}

#[test]
fn select_all_spans_buffer() {
    let mut b = Buffer::from_text("abc\ndef");
    b.select_all();
    let sel = b.selection();
    assert_eq!((sel.anchor, sel.head), (0, b.len_bytes()));
}
