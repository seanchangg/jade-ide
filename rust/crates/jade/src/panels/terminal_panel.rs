//! TERMINAL panel (feature inventory §5.2, Phase-4 wave 2).
//!
//! Renders a [`forge_term::GridSnapshot`] as monospace rows and forwards
//! keystrokes to the PTY. The color mapping (`Named`/`Indexed` ANSI → the
//! hardcoded forge-dark palette, `Default` → theme fg/bg, inverse-video swap)
//! and the key→bytes encoding are factored into pure, unit-tested functions;
//! the renderer is a thin projection over them.
//!
//! # forge-term contract (see `crates/forge-term`)
//! - on `TermEvent::Damaged` → `snapshot(id)` → repaint (cached in `JadeApp`);
//! - on `TermEvent::Exited` → a dim `[exited <code>]` line;
//! - `resize(cols, rows)` on panel resize (computed from pixel bounds here);
//! - `write(bytes)` for keystrokes (this module's [`key_to_bytes`]).
//!
//! # Why `[forge]` status lines are NOT echoed here
//! The TS `writeOutput` wrote to the xterm.js *display buffer*. `forge-term`
//! exposes only `write()`, which feeds the **PTY** (i.e. the shell's stdin), so
//! injecting `[forge] …` text there would run it as a shell command, not print
//! it. There is no display-inject API. So build/run status lines stay in the
//! plain output scrollback, kept as the bottom panel's OUTPUT view toggle
//! (`app.rs`), and the TERMINAL view is a real interactive shell.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use forge_term::{Cell, Color, GridSnapshot, TermId, TermManager};
use gpui::{
    canvas, div, prelude::*, px, rgb, Bounds, Context, Div, FocusHandle, FontWeight, MouseButton,
    Pixels,
};

use crate::app::JadeApp;

/// Cell metrics used to derive cols×rows from the rendered pixel bounds. Height
/// matches the per-row line height; width is a constant close to Menlo's 'M'
/// advance at this size (measuring per-frame isn't worth a text-system round
/// trip for a character-addressed grid).
pub const CELL_W: f32 = 8.0;
pub const CELL_H: f32 = 20.0;

/// The 16 ANSI colors, ported from `TERMINAL_THEME_DARK`
/// (`src/renderer/panels/terminal-panel.ts:13-46`). Index 0..=7 normal, 8..=15
/// bright; the forge-dark chrome reuses editor hues so program output matches.
pub const ANSI_DARK: [u32; 16] = [
    0x2B2D30, // 0 black
    0xCF6B6B, // 1 red
    0x56B389, // 2 green
    0xD4A76A, // 3 yellow
    0x8DB2FF, // 4 blue
    0x9BB5CF, // 5 magenta
    0x7A9484, // 6 cyan
    0xDFE1E5, // 7 white
    0x6F737A, // 8 bright black
    0xCF6B6B, // 9 bright red
    0x56B389, // 10 bright green
    0xD4A76A, // 11 bright yellow
    0x8DB2FF, // 12 bright blue
    0x9BB5CF, // 13 bright magenta
    0x7A9484, // 14 bright cyan
    0xDFE1E5, // 15 bright white
];

/// The 16 ANSI colors for forge-light, ported from `TERMINAL_THEME_LIGHT`
/// (`src/renderer/panels/terminal-panel.ts:28-40`): the same hues darkened for
/// contrast on the cream background. Swapped in on the theme toggle (§5.2 — a
/// canvas terminal can't read CSS vars, so the palette is chosen in code).
pub const ANSI_LIGHT: [u32; 16] = [
    0x1F2328, // 0 black
    0xB3403F, // 1 red
    0x2E7D5B, // 2 green
    0x9A6700, // 3 yellow
    0x2F5FD0, // 4 blue
    0x6E5494, // 5 magenta
    0x3E6D5D, // 6 cyan
    0xE8EAED, // 7 white
    0x6F737A, // 8 bright black
    0xB3403F, // 9 bright red
    0x2E7D5B, // 10 bright green
    0x9A6700, // 11 bright yellow
    0x2F5FD0, // 12 bright blue
    0x6E5494, // 13 bright magenta
    0x3E6D5D, // 14 bright cyan
    0xF5F6F8, // 15 bright white
];

/// forge-dark terminal default fg / bg (the palette's `foreground`/`background`).
pub const TERM_DEFAULT_FG: u32 = 0xDFE1E5;
pub const TERM_DEFAULT_BG: u32 = 0x1E1F22;

/// The active 16-color ANSI palette for a theme (`is_light` selects the light
/// port). The `Color::Default` fg/bg still come from the `Theme` surfaces.
pub fn ansi_palette(is_light: bool) -> &'static [u32; 16] {
    if is_light {
        &ANSI_LIGHT
    } else {
        &ANSI_DARK
    }
}

/// Resolve one raw [`Color`] into an `0xRRGGBB` value against a specific ANSI
/// `palette`. `Named`/`Indexed` map through it (256-color cube/greyscale computed
/// for 16..=255), `Rgb` passes through, `Default` yields the theme default.
pub fn resolve_color_pal(c: Color, default: u32, palette: &[u32; 16]) -> u32 {
    match c {
        Color::Named(i) => palette.get(i as usize).copied().unwrap_or(default),
        Color::Indexed(i) => indexed_rgb(i, default, palette),
        Color::Rgb(r, g, b) => rgb_u32(r, g, b),
        Color::Default => default,
    }
}

/// [`resolve_color_pal`] against the forge-dark palette (back-compat + tests).
pub fn resolve_color(c: Color, default: u32) -> u32 {
    resolve_color_pal(c, default, &ANSI_DARK)
}

fn rgb_u32(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// xterm 256-color resolution: 0..=15 the ANSI palette, 16..=231 the 6×6×6 RGB
/// cube, 232..=255 the 24-step greyscale ramp.
fn indexed_rgb(i: u8, default: u32, palette: &[u32; 16]) -> u32 {
    match i {
        0..=15 => palette[i as usize],
        16..=231 => {
            let n = i - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            rgb_u32(step(r), step(g), step(b))
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            rgb_u32(v, v, v)
        }
        // exhaustive but keep the default arm for clarity
        #[allow(unreachable_patterns)]
        _ => default,
    }
}

/// Resolve a cell's (fg, bg) against a specific ANSI `palette`, honoring inverse
/// video by swapping. `def_fg`/`def_bg` are the theme defaults for `Color::Default`.
pub fn cell_colors_pal(cell: &Cell, def_fg: u32, def_bg: u32, palette: &[u32; 16]) -> (u32, u32) {
    let fg = resolve_color_pal(cell.fg, def_fg, palette);
    let bg = resolve_color_pal(cell.bg, def_bg, palette);
    if cell.flags.inverse {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

/// [`cell_colors_pal`] against the forge-dark palette (back-compat + tests).
pub fn cell_colors(cell: &Cell, def_fg: u32, def_bg: u32) -> (u32, u32) {
    cell_colors_pal(cell, def_fg, def_bg, &ANSI_DARK)
}

/// Modifier state for [`key_to_bytes`] (a testable subset of `gpui::Modifiers`).
#[derive(Clone, Copy, Default, Debug)]
pub struct KeyMods {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    /// cmd (macOS) — reserved for app shortcuts, never sent to the PTY.
    pub platform: bool,
}

/// Encode a GPUI keystroke into the bytes to `write()` to the PTY. Returns
/// `None` for keys that shouldn't reach the shell (cmd-chords, unmapped keys).
///
/// - Enter → `\r`, Backspace → `0x7f`, Tab → `\t`, Escape → `0x1b`, Space → ` `.
/// - Arrows → CSI (`ESC [ A/B/C/D`); Home/End → `ESC [ H/F`.
/// - Ctrl+letter → the C0 control code (`Ctrl+C` → `0x03`).
/// - Otherwise the printable `key_char` (which already reflects Shift), else a
///   single-character `key`.
pub fn key_to_bytes(key: &str, key_char: Option<&str>, mods: KeyMods) -> Option<Vec<u8>> {
    // cmd-chords (copy/paste/etc.) are app-level, not shell input.
    if mods.platform {
        return None;
    }

    // Ctrl+letter → C0 control byte (Ctrl+C=3, Ctrl+D=4, Ctrl+Z=26, …).
    if mods.control && key.chars().count() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            return Some(vec![(ch.to_ascii_lowercase() as u8) & 0x1f]);
        }
    }

    match key {
        "enter" => return Some(vec![b'\r']),
        "backspace" => return Some(vec![0x7f]),
        "tab" => return Some(vec![b'\t']),
        "escape" => return Some(vec![0x1b]),
        "space" => return Some(vec![b' ']),
        "up" => return Some(b"\x1b[A".to_vec()),
        "down" => return Some(b"\x1b[B".to_vec()),
        "right" => return Some(b"\x1b[C".to_vec()),
        "left" => return Some(b"\x1b[D".to_vec()),
        "home" => return Some(b"\x1b[H".to_vec()),
        "end" => return Some(b"\x1b[F".to_vec()),
        _ => {}
    }

    // Printable input (no control chord). key_char already accounts for Shift.
    if !mods.control {
        if let Some(kc) = key_char {
            if !kc.is_empty() {
                return Some(kc.as_bytes().to_vec());
            }
        }
        if key.chars().count() == 1 {
            return Some(key.as_bytes().to_vec());
        }
    }
    None
}

/// A maximal run of adjacent cells sharing fg/bg/weight — rendered as one span
/// so a row is a handful of divs, not one-per-column.
struct Run {
    text: String,
    fg: u32,
    bg: u32,
    bold: bool,
}

fn row_runs(cells: &[Cell], def_fg: u32, def_bg: u32, palette: &[u32; 16]) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for cell in cells {
        let (fg, bg) = cell_colors_pal(cell, def_fg, def_bg, palette);
        let bold = cell.flags.bold;
        match runs.last_mut() {
            Some(r) if r.fg == fg && r.bg == bg && r.bold == bold => r.text.push(cell.ch),
            _ => runs.push(Run {
                text: cell.ch.to_string(),
                fg,
                bg,
                bold,
            }),
        }
    }
    runs
}

/// Render the terminal body: a resize-measuring canvas underlay plus the grid
/// rows. Focusable so `on_key_down` reaches [`key_to_bytes`] → `write()`.
pub fn render(app: &JadeApp, handle: FocusHandle, _cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = app.theme.clone();
    let def_fg = theme.text;
    let def_bg = theme.bg;
    let palette = ansi_palette(theme.is_light);

    let body = div()
        .id("terminal-body")
        .relative()
        .flex_1()
        .w_full()
        .overflow_hidden()
        .bg(rgb(theme.bg))
        .track_focus(&handle)
        .font_family(crate::fonts::mono_family())
        .text_size(px(12.))
        .line_height(px(CELL_H));

    // Focus on click so keystrokes flow to this panel.
    let focus = handle.clone();
    let body = body.on_mouse_down(
        MouseButton::Left,
        move |_ev, window, cx| {
            window.focus(&focus, cx);
        },
    );

    // Keystrokes → PTY bytes (captures the manager + id; no app state needed).
    let manager = app.term.clone();
    let key_id = app.term_id;
    let body = body.on_key_down(move |ev, _window, _cx| {
        let Some(id) = key_id else { return };
        let ks = &ev.keystroke;
        let mods = KeyMods {
            control: ks.modifiers.control,
            alt: ks.modifiers.alt,
            shift: ks.modifiers.shift,
            platform: ks.modifiers.platform,
        };
        if let Some(bytes) = key_to_bytes(&ks.key, ks.key_char.as_deref(), mods) {
            manager.write(id, &bytes);
        }
    });

    // Resize underlay: derive cols×rows from bounds, resize only on change.
    let resize = resize_canvas(app.term.clone(), app.term_id, app.term_last_size.clone());

    let mut grid = div().absolute().top_0().left_0().flex().flex_col();
    if let Some(snap) = &app.term_snapshot {
        grid = render_grid(grid, snap, def_fg, def_bg, palette);
    }
    if app.term_exited {
        let code = app
            .term_exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        grid = grid.child(
            div()
                .text_color(rgb(theme.muted))
                .child(format!("[exited {code}]")),
        );
    }

    body.child(resize).child(grid)
}

fn render_grid(mut grid: Div, snap: &GridSnapshot, def_fg: u32, def_bg: u32, palette: &[u32; 16]) -> Div {
    for row in &snap.cells {
        let runs = row_runs(row, def_fg, def_bg, palette);
        let mut line = div().flex().flex_row().h(px(CELL_H)).items_center();
        for r in runs {
            let mut span = div().text_color(rgb(r.fg)).child(r.text);
            if r.bg != def_bg {
                span = span.bg(rgb(r.bg));
            }
            if r.bold {
                span = span.font_weight(FontWeight::BOLD);
            }
            line = line.child(span);
        }
        grid = grid.child(line);
    }
    grid
}

fn resize_canvas(
    manager: Arc<TermManager>,
    id: Option<TermId>,
    last: Arc<AtomicU32>,
) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, _window, _| {
            let Some(id) = id else { return };
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let cols = ((w / CELL_W).floor() as u16).max(2);
            let rows = ((h / CELL_H).floor() as u16).max(2);
            let packed = ((cols as u32) << 16) | rows as u32;
            // Resize only when the derived geometry actually changed, so the
            // Damaged→snapshot→repaint loop this triggers can't run away.
            if last.swap(packed, Ordering::Relaxed) != packed {
                manager.resize(id, cols, rows);
            }
        },
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_term::CellFlags;

    fn cell(ch: char, fg: Color, bg: Color, inverse: bool) -> Cell {
        Cell {
            ch,
            fg,
            bg,
            flags: CellFlags {
                inverse,
                ..Default::default()
            },
        }
    }

    #[test]
    fn named_and_default_colors() {
        assert_eq!(resolve_color(Color::Named(1), TERM_DEFAULT_FG), 0xCF6B6B); // red
        assert_eq!(resolve_color(Color::Named(2), TERM_DEFAULT_FG), 0x56B389); // green
        assert_eq!(resolve_color(Color::Named(4), TERM_DEFAULT_FG), 0x8DB2FF); // blue
        // Default resolves to whatever theme default is supplied.
        assert_eq!(resolve_color(Color::Default, TERM_DEFAULT_FG), TERM_DEFAULT_FG);
        assert_eq!(resolve_color(Color::Default, 0x123456), 0x123456);
    }

    #[test]
    fn indexed_and_rgb_colors() {
        // Low indices alias the ANSI palette.
        assert_eq!(resolve_color(Color::Indexed(1), TERM_DEFAULT_FG), 0xCF6B6B);
        // Cube corner 16 = (0,0,0); 231 = (255,255,255).
        assert_eq!(resolve_color(Color::Indexed(16), TERM_DEFAULT_FG), 0x000000);
        assert_eq!(resolve_color(Color::Indexed(231), TERM_DEFAULT_FG), 0xFFFFFF);
        // Greyscale ramp start.
        assert_eq!(resolve_color(Color::Indexed(232), TERM_DEFAULT_FG), 0x080808);
        // Truecolor passes through.
        assert_eq!(resolve_color(Color::Rgb(0x12, 0x34, 0x56), 0), 0x123456);
    }

    #[test]
    fn inverse_swaps_fg_bg() {
        let c = cell('x', Color::Named(1), Color::Default, false);
        assert_eq!(cell_colors(&c, TERM_DEFAULT_FG, TERM_DEFAULT_BG), (0xCF6B6B, TERM_DEFAULT_BG));
        let c = cell('x', Color::Named(1), Color::Default, true);
        assert_eq!(cell_colors(&c, TERM_DEFAULT_FG, TERM_DEFAULT_BG), (TERM_DEFAULT_BG, 0xCF6B6B));
    }

    #[test]
    fn key_encoding_specials() {
        let m = KeyMods::default();
        assert_eq!(key_to_bytes("enter", Some("\n"), m), Some(vec![b'\r']));
        assert_eq!(key_to_bytes("backspace", None, m), Some(vec![0x7f]));
        assert_eq!(key_to_bytes("tab", Some("\t"), m), Some(vec![b'\t']));
        assert_eq!(key_to_bytes("escape", None, m), Some(vec![0x1b]));
        assert_eq!(key_to_bytes("space", Some(" "), m), Some(vec![b' ']));
    }

    #[test]
    fn key_encoding_arrows() {
        let m = KeyMods::default();
        assert_eq!(key_to_bytes("up", None, m), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_to_bytes("down", None, m), Some(b"\x1b[B".to_vec()));
        assert_eq!(key_to_bytes("right", None, m), Some(b"\x1b[C".to_vec()));
        assert_eq!(key_to_bytes("left", None, m), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn key_encoding_ctrl_and_printable() {
        let ctrl = KeyMods {
            control: true,
            ..Default::default()
        };
        assert_eq!(key_to_bytes("c", Some("c"), ctrl), Some(vec![0x03])); // Ctrl+C
        assert_eq!(key_to_bytes("d", Some("d"), ctrl), Some(vec![0x04])); // Ctrl+D

        let m = KeyMods::default();
        assert_eq!(key_to_bytes("a", Some("a"), m), Some(b"a".to_vec()));
        assert_eq!(key_to_bytes("!", Some("!"), m), Some(b"!".to_vec())); // shifted char

        // cmd-chords are not shell input.
        let cmd = KeyMods {
            platform: true,
            ..Default::default()
        };
        assert_eq!(key_to_bytes("c", Some("c"), cmd), None);
    }

    #[test]
    fn row_runs_group_adjacent() {
        let cells = vec![
            cell('a', Color::Named(1), Color::Default, false),
            cell('b', Color::Named(1), Color::Default, false),
            cell('c', Color::Named(2), Color::Default, false),
        ];
        let runs = row_runs(&cells, TERM_DEFAULT_FG, TERM_DEFAULT_BG, &ANSI_DARK);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "ab");
        assert_eq!(runs[1].text, "c");
    }

    #[test]
    fn light_ansi_mapping() {
        let light = ansi_palette(true);
        assert_eq!(light, &ANSI_LIGHT);
        // Named colors resolve through the light palette (darkened hues).
        assert_eq!(resolve_color_pal(Color::Named(1), 0, light), 0xB3403F); // red
        assert_eq!(resolve_color_pal(Color::Named(2), 0, light), 0x2E7D5B); // green
        assert_eq!(resolve_color_pal(Color::Named(4), 0, light), 0x2F5FD0); // blue
        // ANSI black (index 0) is the dark charcoal fg on light, not near-black bg.
        assert_eq!(resolve_color_pal(Color::Named(0), 0, light), 0x1F2328);
        // Indexed 0..=15 also route through the light palette.
        assert_eq!(resolve_color_pal(Color::Indexed(3), 0, light), 0x9A6700); // yellow
        // Dark palette still selected when not light.
        assert_eq!(ansi_palette(false), &ANSI_DARK);
        assert_eq!(resolve_color_pal(Color::Named(1), 0, &ANSI_DARK), 0xCF6B6B);
        // Default falls back to the supplied theme default regardless of palette.
        assert_eq!(resolve_color_pal(Color::Default, 0x123456, light), 0x123456);
    }
}
