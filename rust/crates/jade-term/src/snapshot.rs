//! Renderer-agnostic snapshot types.
//!
//! A [`GridSnapshot`] is a cheap, owned copy of the terminal grid taken behind
//! the state mutex. The future GPUI panel consumes these instead of touching
//! `alacritty_terminal` types directly, so the render layer never links the
//! terminal state machine's internals.
//!
//! Colors are reported *raw* — exactly what the VTE parser recorded for the
//! cell — as a small [`Color`] enum. The renderer maps `Named`/`Indexed` into
//! the theme's ANSI palette (the C++ IDE kept hardcoded dark/light palettes,
//! see `jade-feature-inventory.md` §5.2) and resolves `Default` to the
//! theme's default fg/bg. We deliberately do *not* resolve colors here.

use alacritty_terminal::vte::ansi::{Color as VteColor, NamedColor};

/// A single cell's foreground or background color, as recorded by the parser.
///
/// The renderer resolves these against the active theme; `jade-term` never
/// bakes in a palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// One of the 16 standard ANSI colors, by index 0..=15
    /// (0 black, 1 red, … 7 white, 8 bright-black, … 15 bright-white).
    Named(u8),
    /// A color from the 256-color cube/greyscale palette, index 0..=255.
    Indexed(u8),
    /// A 24-bit truecolor value (`COLORTERM=truecolor` is advertised).
    Rgb(u8, u8, u8),
    /// The terminal default fg/bg (theme-defined by the renderer).
    Default,
}

impl From<VteColor> for Color {
    fn from(c: VteColor) -> Self {
        match c {
            VteColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
            VteColor::Indexed(i) => Color::Indexed(i),
            VteColor::Named(named) => {
                // The 16 base ANSI colors are discriminants 0..=15 and map to a
                // stable index. Everything else (Foreground/Background/Cursor and
                // the dim/bright *semantic* slots at 256+) is the terminal
                // default from the renderer's point of view.
                let idx = named as usize;
                if idx < 16 {
                    Color::Named(idx as u8)
                } else {
                    match named {
                        // Bright/dim foreground still means "the default fg".
                        NamedColor::Foreground
                        | NamedColor::BrightForeground
                        | NamedColor::DimForeground
                        | NamedColor::Background
                        | NamedColor::Cursor => Color::Default,
                        _ => Color::Default,
                    }
                }
            }
        }
    }
}

/// Text-style flags for a cell. Only the attributes the renderer needs are
/// surfaced; `alacritty_terminal` tracks more (strikeout, undercurl, …) which
/// can be added when the panel grows to use them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellFlags {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Reverse video — the renderer should swap fg/bg for this cell.
    pub inverse: bool,
}

/// One grid cell: a character plus its resolved-to-raw colors and flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', fg: Color::Default, bg: Color::Default, flags: CellFlags::default() }
    }
}

/// Cursor position and visibility at snapshot time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Column, 0-based, within the visible viewport.
    pub col: usize,
    /// Line, 0-based, within the visible viewport (0 = top visible row).
    pub line: usize,
    /// Whether the cursor should be drawn (DECTCEM / `?25h`/`?25l`).
    pub visible: bool,
}

/// An owned copy of the terminal grid: the visible viewport plus a bounded
/// window of scrollback above it.
///
/// Cheap to build at the sizes we care about (80x24 … 300x100), taken while the
/// [`crate::Term`] mutex is held and handed to the renderer by value.
#[derive(Debug, Clone)]
pub struct GridSnapshot {
    /// Number of columns in the viewport.
    pub cols: usize,
    /// Number of rows in the visible viewport.
    pub rows: usize,
    /// Cursor state (position is relative to the visible viewport).
    pub cursor: Cursor,
    /// Visible viewport, top row first; each inner `Vec` has `cols` cells.
    pub cells: Vec<Vec<Cell>>,
    /// Scrollback lines *above* the viewport, oldest first. At most the
    /// configured scrollback limit (default 1000). Each inner `Vec` is a full
    /// row of `cols` cells.
    pub scrollback: Vec<Vec<Cell>>,
    /// DECCKM application cursor keys mode (`?1h`): arrows should be encoded
    /// as SS3 (`ESC O A`…) instead of CSI (`ESC [ A`…).
    pub app_cursor: bool,
    /// Bracketed paste mode (`?2004h`): pasted text should be wrapped in
    /// `ESC [ 200~` … `ESC [ 201~` so TUIs can tell paste from typing.
    pub bracketed_paste: bool,
}

impl GridSnapshot {
    /// Convenience for tests/renderers: does any cell in the viewport or
    /// scrollback contain the given substring on a single row?
    pub fn contains_text(&self, needle: &str) -> bool {
        let row_has = |row: &Vec<Cell>| -> bool {
            let s: String = row.iter().map(|c| c.ch).collect();
            s.contains(needle)
        };
        self.cells.iter().any(row_has) || self.scrollback.iter().any(row_has)
    }
}
