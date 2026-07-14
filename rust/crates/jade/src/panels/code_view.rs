//! Read-only code viewer (deliverable §5).
//!
//! A virtualized line list (`uniform_list`) so a 5k-line file renders only the
//! visible rows, never 5k divs/frame. Each row is a fixed-height (20px) line: a
//! right-aligned gutter line number (#4B4E56) plus the code, rendered as a single
//! [`StyledText`] whose per-token color runs come from the tab's precomputed
//! highlight spans (`with_highlights`; gaps fall back to the default text color).
//! Menlo 13px, line-height 20, padding-top 16, horizontal overflow clipped
//! (no wrap).

use std::collections::HashMap;

use gpui::{div, prelude::*, px, rgb, rgba, uniform_list, ClickEvent, Context, HighlightStyle, Rgba};

use crate::app::JadeApp;
use crate::decorations::flow::GlyphKind;
use crate::decorations::{self, RuntimeAlloc};
use crate::theme::Theme;

/// Editor metrics (§4.1 "editor look").
const FONT_PX: f32 = 13.0;
const LINE_H: f32 = 20.0;
const PAD_TOP: f32 = 16.0;
const GUTTER_W: f32 = 52.0;
/// Width of the flow glyph margin column (shown only when Flow is toggled on).
const GLYPH_W: f32 = 16.0;
/// End-of-line annotation text size (§4.5: 11px emerald @0.7 opacity).
const ANN_PX: f32 = 11.0;

/// Gutter line-number colors (§4.1).
const GUTTER_FG: u32 = 0x4B4E56;

// ── Flow decoration colors (§4.8 FLOW_COLORS) ──────────────────────────────
const FLOW_SEQ: u32 = 0x56B389; // emerald — sequential
const FLOW_CALL: u32 = 0x8DB2FF; // periwinkle — call / loop-back
const FLOW_RETURN: u32 = 0xD4A76A; // amber — return / branch
const FLOW_ERROR: u32 = 0xCF6B6B; // red — error
/// Static/exec/runtime annotation color (§4.5: emerald #56B389).
const ANN_EMERALD: u32 = 0x56B389;

/// A packed `0xRRGGBB` with an alpha fraction → `gpui::Rgba`.
fn rgba_a(rgb: u32, alpha: f32) -> Rgba {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    rgba((rgb << 8) | (a & 0xff))
}

/// Base color for a flow glyph kind.
fn flow_color(kind: GlyphKind) -> u32 {
    match kind {
        GlyphKind::Sequential => FLOW_SEQ,
        GlyphKind::Call | GlyphKind::Loop => FLOW_CALL,
        GlyphKind::Return | GlyphKind::Branch => FLOW_RETURN,
        GlyphKind::Error => FLOW_ERROR,
    }
}

/// Whole-line (bg alpha, left-border alpha) for a flow kind (§4.8 CSS `:87-110`).
fn flow_tint(kind: GlyphKind) -> (f32, f32) {
    match kind {
        GlyphKind::Call => (0.08, 0.375), // rgba .08 / border @60
        GlyphKind::Error => (0.12, 0.5),  // rgba .12 / border @80
        _ => (0.06, 0.25),                // rgba .06 / border @40
    }
}

/// Render the viewer for the active tab, or a centered placeholder when none is
/// open.
pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = app.theme.clone();

    let Some(tab) = app.editor.active_tab() else {
        return div()
            .flex()
            .flex_1()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_color(rgb(theme.muted))
                    .child("No file open — pick one from the tree"),
            );
    };

    let line_count = tab.lines.len();
    let default_color = theme.text;

    div().flex().flex_1().size_full().child(
        uniform_list(
            "code-lines",
            line_count,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                let mut rows = Vec::with_capacity(range.len());

                // Dynamic state read once per range render (all disjoint fields).
                let flow_visible = this.flow_visible;
                let error_line = this.error_line;
                let has_run = !this.last_executed.is_empty();

                let Some(tab) = this.editor.active_tab() else {
                    return rows;
                };

                // Build the runtime-allocation map for the active file once,
                // matching per_line keys by full path or basename (the alloc
                // event's `file` form isn't guaranteed).
                let runtime = runtime_allocs_for(this, tab);

                for i in range {
                    let line_no = i + 1;
                    let text = tab.lines.get(i).cloned().unwrap_or_default();
                    let spans = tab.highlights.get(i);

                    // Per-token color runs; gaps inherit the default text color.
                    let highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = spans
                        .map(|ss| {
                            ss.iter()
                                .filter(|s| s.end <= text.len())
                                .map(|s| {
                                    (
                                        s.start..s.end,
                                        HighlightStyle {
                                            color: Some(rgb(s.color).into()),
                                            ..Default::default()
                                        },
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let styled = gpui::StyledText::new(text).with_highlights(highlights);

                    // ── Merged end-of-line annotation (§4.5) ──────────────────
                    let size = tab.sizes.get(&line_no).map(String::as_str);
                    let exec = this
                        .last_executed
                        .get(&(line_no as u32))
                        .and_then(|&c| {
                            let prev = this
                                .prev_executed
                                .get(&(line_no as u32))
                                .copied()
                                .unwrap_or(0);
                            decorations::exec_annotations::annotate_line(c, prev)
                        });
                    let rt: Option<RuntimeAlloc> = runtime.get(&(line_no as u32)).copied();
                    let annotation =
                        decorations::merge_annotation(size, exec.as_deref(), rt.as_ref());

                    // ── Flow glyph + whole-line tint (§4.8) ───────────────────
                    let mut row_bg: Option<Rgba> = None;
                    let mut border_col = rgba_a(0, 0.0); // transparent, keeps 2px width
                    let mut glyph: Option<(char, Rgba)> = None;
                    let mut nav_target: Option<usize> = None;

                    if flow_visible {
                        let executed = has_run && this.last_executed.contains_key(&(line_no as u32));
                        if let Some(fg) = tab.flow.glyphs.get(&line_no) {
                            nav_target = fg.targets.first().copied();
                            let color = flow_color(fg.kind);
                            let (ba, bo) = flow_tint(fg.kind);
                            let mut gcolor = rgba_a(color, 1.0);
                            if has_run {
                                if executed {
                                    row_bg = Some(rgba_a(FLOW_SEQ, 0.08)); // executed green
                                    border_col = rgba_a(color, bo);
                                } else {
                                    gcolor = rgba_a(color, 0.25); // dim off-path glyph
                                }
                            } else {
                                row_bg = Some(rgba_a(color, ba));
                                border_col = rgba_a(color, bo);
                            }
                            glyph = Some((fg.kind.glyph(), gcolor));
                        } else if executed {
                            row_bg = Some(rgba_a(FLOW_SEQ, 0.04)); // executed, non-flow line
                        }

                        // Error line overrides everything.
                        if error_line == Some(line_no as u32) {
                            row_bg = Some(rgba_a(FLOW_ERROR, 0.12));
                            border_col = rgba_a(FLOW_ERROR, 0.5);
                            glyph = Some((GlyphKind::Error.glyph(), rgba_a(FLOW_ERROR, 1.0)));
                            nav_target = None;
                        }
                    }

                    // ── Row assembly ──────────────────────────────────────────
                    let mut row = div().flex().flex_row().h(px(LINE_H)).items_center();

                    if flow_visible {
                        let mut cell = div()
                            .id(("flow-glyph", i))
                            .w(px(GLYPH_W))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.));
                        if let Some((ch, col)) = glyph {
                            cell = cell.text_color(col).child(ch.to_string());
                        }
                        // Cmd/Ctrl+Click a glyph line navigates to its target.
                        if let Some(target) = nav_target {
                            cell = cell.cursor_pointer().on_click(cx.listener(
                                move |app: &mut JadeApp, ev: &ClickEvent, _win, cx| {
                                    let m = ev.modifiers();
                                    if m.platform || m.control {
                                        app.flow_goto(target);
                                        cx.notify();
                                    }
                                },
                            ));
                        }
                        row = row.child(cell);
                    }

                    // Always reserve a 2px left border so tinted lines don't shift.
                    row = row.border_l_2().border_color(border_col);
                    if let Some(bg) = row_bg {
                        row = row.bg(bg);
                    }

                    row = row
                        .child(
                            // Gutter: right-aligned line number.
                            div()
                                .w(px(GUTTER_W))
                                .flex_none()
                                .pr(px(8.))
                                .text_right()
                                .text_color(rgb(GUTTER_FG))
                                .child(line_no.to_string()),
                        )
                        .child({
                            // Code cell: code text, then the trailing annotation.
                            let mut cell = div()
                                .flex_1()
                                .overflow_hidden()
                                .flex()
                                .flex_row()
                                .items_center()
                                .child(
                                    div()
                                        .whitespace_nowrap()
                                        .flex_none()
                                        .text_color(rgb(default_color))
                                        .child(styled),
                                );
                            if let Some(ann) = annotation {
                                cell = cell.child(
                                    div()
                                        .flex_none()
                                        .whitespace_nowrap()
                                        .text_size(px(ANN_PX))
                                        .text_color(rgba_a(ANN_EMERALD, 0.7))
                                        .child(ann),
                                );
                            }
                            cell
                        });

                    rows.push(row);
                }
                rows
            }),
        )
        .track_scroll(&app.code_scroll)
        .flex_1()
        .size_full()
        .pt(px(PAD_TOP))
        .px(px(8.))
        .text_size(px(FONT_PX))
        .line_height(px(LINE_H))
        .font_family("Menlo"),
    )
}

/// Build the `line → RuntimeAlloc` map for the active tab's file from the
/// memory-bar per-`file:line` tracker (§4.5 system 3). Keys are matched by full
/// path or basename, since the alloc event's `file` field form isn't fixed.
fn runtime_allocs_for(app: &JadeApp, tab: &crate::editor_view::OpenTab) -> HashMap<u32, RuntimeAlloc> {
    let mut out = HashMap::new();
    if app.mem.per_line.is_empty() {
        return out;
    }
    let tab_disp = tab.path.to_string_lossy();
    let tab_base = tab
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string);
    for (key, la) in &app.mem.per_line {
        let Some((file, lnum)) = key.rsplit_once(':') else {
            continue;
        };
        let Ok(line) = lnum.parse::<u32>() else {
            continue;
        };
        let base = std::path::Path::new(file)
            .file_name()
            .and_then(|n| n.to_str());
        let matches = file == tab_disp || (tab_base.as_deref().is_some() && tab_base.as_deref() == base);
        if matches {
            out.insert(
                line,
                RuntimeAlloc {
                    calls: la.calls,
                    bytes: la.bytes,
                    leaked: la.leaked,
                },
            );
        }
    }
    out
}

/// The tab strip above the viewer (deliverable §3): one chip per open tab with a
/// close `×`, the active tab underlined. Middle-click also closes (GPUI exposes
/// the mouse button on the down event).
pub fn tab_strip(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    let mut strip = div()
        .id("tab-strip")
        .flex()
        .flex_row()
        .items_center()
        .h(px(30.))
        .w_full()
        .gap(px(2.))
        .px(px(4.))
        .bg(rgb(theme.panel))
        .border_b_1()
        .border_color(rgb(theme.border))
        .overflow_x_hidden();

    for (i, tab) in app.editor.tabs.iter().enumerate() {
        let active = app.editor.active == Some(i);
        strip = strip.child(tab_chip(i, &tab.name, active, theme, cx));
    }
    strip
}

fn tab_chip(
    index: usize,
    name: &str,
    active: bool,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let fg = if active { theme.text } else { theme.muted };

    let mut chip = div()
        .id(("tab", index))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(26.))
        .px(px(8.))
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .text_color(rgb(fg))
        .bg(rgb(theme.bg))
        // Middle-click closes (mouse-down carries the button).
        .on_mouse_down(
            gpui::MouseButton::Middle,
            cx.listener(move |app, _ev, _win, cx| {
                app.close_tab(index);
                cx.notify();
            }),
        )
        // Left-click switches to this tab.
        .on_click(cx.listener(move |app, _ev, _win, cx| {
            app.switch_tab(index);
            cx.notify();
        }))
        .child(div().child(name.to_string()));

    // Active underline (§4.1).
    if active {
        chip = chip.border_b_2().border_color(rgb(theme.accent));
    }

    // Close button.
    chip.child(
        div()
            .id(("tab-close", index))
            .px_1()
            .text_color(rgb(theme.muted))
            .cursor_pointer()
            .on_click(cx.listener(move |app, _ev, _win, cx| {
                app.close_tab(index);
                cx.notify();
            }))
            .child("×"),
    )
}
