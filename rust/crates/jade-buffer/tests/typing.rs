//! Typing helpers: bracket auto-close, type-over, auto-surround, quote rules,
//! Enter auto-indent, tab-as-spaces, and the grapheme-aware deletes.

use jade_buffer::{Buffer, Selection};

#[test]
fn typing_plain_char_inserts_and_advances() {
    let mut b = Buffer::from_text("");
    b.type_char('x');
    assert_eq!(b.to_string(), "x");
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn auto_close_bracket_inserts_pair_caret_between() {
    let mut b = Buffer::from_text("");
    b.type_char('(');
    assert_eq!(b.to_string(), "()");
    assert_eq!(b.selection().caret(), 1); // between the pair
}

#[test]
fn auto_close_all_bracket_kinds() {
    for (open, closed) in [('(', ")"), ('[', "]"), ('{', "}")] {
        let mut b = Buffer::from_text("");
        b.type_char(open);
        assert_eq!(b.to_string(), format!("{open}{closed}"));
        assert_eq!(b.selection().caret(), 1);
    }
}

#[test]
fn type_over_closing_bracket_is_noop_edit() {
    let mut b = Buffer::from_text("");
    b.type_char('('); // "()", caret 1
    let rec = b.type_char(')'); // type over the ')'
    assert_eq!(b.to_string(), "()"); // no new text
    assert_eq!(b.selection().caret(), 2);
    assert!(rec.is_noop());
    assert_eq!(rec.version, b.version()); // version not bumped
}

#[test]
fn type_close_when_not_matching_inserts() {
    let mut b = Buffer::from_text("x");
    b.set_caret(1);
    b.type_char(')'); // nothing to type over → literal insert
    assert_eq!(b.to_string(), "x)");
}

#[test]
fn quote_auto_closes_in_empty_context() {
    let mut b = Buffer::from_text("");
    b.type_char('"');
    assert_eq!(b.to_string(), "\"\"");
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn quote_does_not_double_after_word_char() {
    // Apostrophe in `don't` must not auto-close.
    let mut b = Buffer::from_text("don");
    b.set_caret(3);
    b.type_char('\'');
    assert_eq!(b.to_string(), "don'");
    assert_eq!(b.selection().caret(), 4);
}

#[test]
fn quote_type_over() {
    let mut b = Buffer::from_text("");
    b.type_char('"'); // "\"\"" caret 1
    b.type_char('"'); // type over
    assert_eq!(b.to_string(), "\"\"");
    assert_eq!(b.selection().caret(), 2);
}

#[test]
fn auto_surround_selection_with_bracket() {
    let mut b = Buffer::from_text("abc");
    b.set_selection(Selection::new(0, 3));
    b.type_char('(');
    assert_eq!(b.to_string(), "(abc)");
    // Selection preserved around the inner text.
    let sel = b.selection();
    assert_eq!((sel.anchor, sel.head), (1, 4));
}

#[test]
fn auto_surround_selection_with_quote() {
    let mut b = Buffer::from_text("abc");
    b.set_selection(Selection::new(0, 3));
    b.type_char('"');
    assert_eq!(b.to_string(), "\"abc\"");
    let sel = b.selection();
    assert_eq!((sel.anchor, sel.head), (1, 4));
}

#[test]
fn typing_over_selection_with_plain_char_replaces() {
    let mut b = Buffer::from_text("abc");
    b.set_selection(Selection::new(0, 3));
    b.type_char('z');
    assert_eq!(b.to_string(), "z");
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn enter_copies_previous_line_indentation() {
    let mut b = Buffer::from_text("    foo");
    b.set_caret(7);
    b.insert_newline();
    assert_eq!(b.to_string(), "    foo\n    ");
    assert_eq!(b.line(1), "    ");
    assert_eq!(b.selection().caret(), 12);
}

#[test]
fn enter_with_no_indent() {
    let mut b = Buffer::from_text("foo");
    b.set_caret(3);
    b.insert_newline();
    assert_eq!(b.to_string(), "foo\n");
}

#[test]
fn enter_splits_line_and_indents() {
    let mut b = Buffer::from_text("  ab");
    b.set_caret(3); // between a and b
    b.insert_newline();
    assert_eq!(b.to_string(), "  a\n  b");
}

#[test]
fn tab_inserts_spaces_to_next_tab_stop() {
    let mut b = Buffer::from_text("");
    b.insert_tab();
    assert_eq!(b.to_string(), "    "); // col 0 → 4 spaces
}

#[test]
fn tab_aligns_to_tab_stop_from_mid_column() {
    let mut b = Buffer::from_text("ab");
    b.set_caret(2); // col 2 → 2 spaces to next stop
    b.insert_tab();
    assert_eq!(b.to_string(), "ab  ");
    assert_eq!(b.selection().caret(), 4);
}

#[test]
fn tab_from_col_one() {
    let mut b = Buffer::from_text("a");
    b.set_caret(1); // col 1 → 3 spaces
    b.insert_tab();
    assert_eq!(b.to_string(), "a   ");
}

#[test]
fn delete_backward_removes_grapheme() {
    let mut b = Buffer::from_text("a🎉");
    b.set_caret(5); // after emoji
    b.delete_backward();
    assert_eq!(b.to_string(), "a"); // whole emoji gone
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn delete_backward_removes_selection() {
    let mut b = Buffer::from_text("hello");
    b.set_selection(Selection::new(1, 4));
    b.delete_backward();
    assert_eq!(b.to_string(), "ho");
    assert_eq!(b.selection().caret(), 1);
}

#[test]
fn delete_backward_at_start_is_noop() {
    let mut b = Buffer::from_text("abc");
    b.set_caret(0);
    b.delete_backward();
    assert_eq!(b.to_string(), "abc");
}

#[test]
fn delete_forward_removes_grapheme() {
    let mut b = Buffer::from_text("🎉a");
    b.set_caret(0);
    b.delete_forward();
    assert_eq!(b.to_string(), "a");
    assert_eq!(b.selection().caret(), 0);
}

#[test]
fn delete_word_back_removes_previous_word() {
    let mut b = Buffer::from_text("foo bar");
    b.set_caret(7);
    b.delete_word_back();
    assert_eq!(b.to_string(), "foo ");
    assert_eq!(b.selection().caret(), 4);
}

#[test]
fn delete_word_back_stops_at_punctuation() {
    let mut b = Buffer::from_text("foo.bar");
    b.set_caret(7);
    b.delete_word_back(); // removes "bar"
    assert_eq!(b.to_string(), "foo.");
    b.delete_word_back(); // removes "."
    assert_eq!(b.to_string(), "foo");
}

#[test]
fn typed_text_is_undoable_as_a_unit() {
    let mut b = Buffer::from_text("");
    b.type_char('(');
    assert_eq!(b.to_string(), "()");
    b.undo();
    assert_eq!(b.to_string(), "");
}

#[test]
fn insert_newline_then_delete_backward_roundtrips() {
    let mut b = Buffer::from_text("ab");
    b.set_caret(1);
    b.insert_newline();
    assert_eq!(b.to_string(), "a\nb");
    b.delete_backward();
    assert_eq!(b.to_string(), "ab");
}
