//! Coordinate types. The buffer's canonical position is a **byte offset** into
//! the UTF-8 text; the two structs here are the two *named-coordinate* views the
//! rest of the editor speaks:
//!
//! * [`Point`] — `row` + **char** column, for rendering / caret display. `col`
//!   counts Unicode scalar values (chars), which is what a monospace grid draws.
//! * [`LspPosition`] — `line` + **UTF-16 code-unit** column, the wire coordinate
//!   for `textDocument/didChange`, hover, completion, etc. clangd speaks UTF-16.
//!
//! Keeping them as distinct types (rather than one `Point` with an ambiguous
//! `col`) makes it impossible to accidentally hand a char column to the LSP or a
//! UTF-16 column to the renderer.

/// A row + **char**-column position. `col` is a count of Unicode scalar values
/// from the start of `row` (newline excluded). This is the render/caret view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Point {
    pub row: usize,
    pub col: usize,
}

impl Point {
    pub fn new(row: usize, col: usize) -> Self {
        Point { row, col }
    }
}

/// An LSP text-document position: 0-based `line`, and `character` as a count of
/// **UTF-16 code units** from the line start. This is the shape clangd expects
/// in `Position` and the shape [`crate::EditRecord`] carries for `didChange`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct LspPosition {
    pub line: usize,
    pub character: usize,
}

impl LspPosition {
    pub fn new(line: usize, character: usize) -> Self {
        LspPosition { line, character }
    }
}
