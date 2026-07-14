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

use forge_buffer::Point;
use gpui::{
    canvas, div, point, prelude::*, px, rgb, rgba, size, uniform_list, Bounds, ClickEvent, Context,
    ElementInputHandler, Entity, FocusHandle, HighlightStyle, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, Rgba, UnderlineStyle,
};

use crate::app::{CompletionState, HoverState, JadeApp};
use crate::decorations::flow::GlyphKind;
use crate::decorations::{self, RuntimeAlloc};
use crate::editor_view::{self, CellStyle};
use crate::theme::Theme;

/// Editor metrics (§4.1 "editor look").
const FONT_PX: f32 = 13.0;
pub const LINE_H: f32 = 20.0;
pub const PAD_TOP: f32 = 16.0;
const GUTTER_W: f32 = 52.0;
/// Width of the flow glyph margin column (shown only when Flow is toggled on).
const GLYPH_W: f32 = 16.0;
/// Width of the fold-chevron column between the gutter and the code.
const FOLD_W: f32 = 12.0;
/// Menlo's char advance at [`FONT_PX`] (Menlo is monospace, ~0.602 em → 7.83px
/// at 13px). Used for caret/selection/popup placement and click→column mapping;
/// the two share this constant so hit-testing stays self-consistent. Wide glyphs
/// (CJK/emoji) count as one column — an accepted, documented approximation.
pub const CHAR_W: f32 = 7.83;
/// End-of-line annotation text size (§4.5: 11px emerald @0.7 opacity).
const ANN_PX: f32 = 11.0;

/// A packed `0xRRGGBB` with an alpha fraction → `gpui::Rgba`.
fn rgba_a(rgb: u32, alpha: f32) -> Rgba {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
    rgba((rgb << 8) | (a & 0xff))
}

/// Base color for a flow glyph kind, resolved from the active [`Theme`] (§4.8
/// FLOW_COLORS map onto accent/periwinkle/amber/red so the light theme applies).
fn flow_color(kind: GlyphKind, theme: &Theme) -> u32 {
    match kind {
        GlyphKind::Sequential => theme.accent,
        GlyphKind::Call | GlyphKind::Loop => theme.periwinkle,
        GlyphKind::Return | GlyphKind::Branch => theme.amber,
        GlyphKind::Error => theme.red,
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

/// Render the editable editor for the active tab, or a centered placeholder when
/// none is open. The surface is focusable (keys → [`JadeApp::editor_key`], text →
/// IME `replace_text_in_range`), mouse-selectable, and overlays the autocomplete
/// + hover popups.
pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
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
            )
            .into_any_element();
    };

    let visible_count = tab.visible_rows().len();
    let default_color = theme.text;
    let flow_visible_now = app.flow_visible;

    // The focus handle backing keyboard + IME input (created lazily in the shell).
    let handle = app
        .editor_focus
        .clone()
        .unwrap_or_else(|| cx.focus_handle());

    let list = uniform_list(
        "code-lines",
        visible_count,
        cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
            let mut rows = Vec::with_capacity(range.len());

            let flow_visible = this.flow_visible;
            let error_line = this.error_line;
            let has_run = !this.last_executed.is_empty();
            let theme = this.theme.clone();
            // Ghost text (§4.11): shown on the caret row only. Multi-line ghosts
            // render their first line inline with a `⏎…` continuation marker; Tab
            // still inserts the full suggestion.
            let ghost_render: Option<(usize, String)> = this.ghost.as_ref().map(|g| {
                let first = g.text.lines().next().unwrap_or("").to_string();
                let shown = if g.text.contains('\n') {
                    format!("{first} ⏎…")
                } else {
                    first
                };
                (g.anchor.0, shown)
            });
            let focused = this
                .editor_focus
                .as_ref()
                .map(|f| f.is_focused(window))
                .unwrap_or(false);
            let row_handle = this.editor_focus.clone();
            let char_w = this.char_w();

            let Some(tab) = this.editor.active_tab() else {
                return rows;
            };

            let runtime = runtime_allocs_for(this, tab);
            let Some(tab) = this.editor.active_tab() else {
                return rows;
            };

            // Caret + selection (buffer byte coordinates).
            let sel = tab.buffer.selection();
            let caret_point = tab.caret_point();

            // Display → buffer row map (folds hide interior rows).
            let visible = tab.visible_rows();

            for di in range {
                let Some(&i) = visible.get(di) else { break };
                let line_no = i + 1;
                let text = tab.line(i);
                let line_bytes = text.len();
                let line_start = tab.buffer.point_to_offset(Point::new(i, 0));
                let line_end = line_start + line_bytes;

                // Style layers → flattened runs (syntax · selection · diagnostics
                // · IME marked), see editor_view::merge_line_styles.
                let syntax = tab.highlights.get(i).map(Vec::as_slice).unwrap_or(&[]);
                let selection = editor_view::selection_on_line(sel, line_start, line_end);
                let underlines = editor_view::diagnostic_underlines_for_row(
                    &tab.diagnostics,
                    i,
                    &tab.buffer,
                    theme.red,
                    theme.amber,
                    theme.periwinkle,
                );
                let marked = tab.marked.as_ref().and_then(|m| {
                    editor_view::selection_on_line(
                        forge_buffer::Selection::new(m.start, m.end),
                        line_start,
                        line_end,
                    )
                });
                let cells =
                    editor_view::merge_line_styles(line_bytes, syntax, selection, &underlines, marked);
                let highlights = cells
                    .into_iter()
                    .map(|(r, s)| (r, cell_to_style(&s, &theme)))
                    .collect::<Vec<_>>();
                let styled = gpui::StyledText::new(text).with_highlights(highlights);

                // Merged end-of-line annotation (§4.5).
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
                let annotation = decorations::merge_annotation(size, exec.as_deref(), rt.as_ref());

                // Flow glyph + whole-line tint (§4.8).
                let mut row_bg: Option<Rgba> = None;
                let mut border_col = rgba_a(0, 0.0);
                let mut glyph: Option<(char, Rgba)> = None;
                let mut nav_target: Option<usize> = None;

                if flow_visible {
                    let executed = has_run && this.last_executed.contains_key(&(line_no as u32));
                    if let Some(fg) = tab.flow.glyphs.get(&line_no) {
                        nav_target = fg.targets.first().copied();
                        let color = flow_color(fg.kind, &theme);
                        let (ba, bo) = flow_tint(fg.kind);
                        let mut gcolor = rgba_a(color, 1.0);
                        if has_run {
                            if executed {
                                row_bg = Some(rgba_a(theme.accent, 0.08));
                                border_col = rgba_a(color, bo);
                            } else {
                                gcolor = rgba_a(color, 0.25);
                            }
                        } else {
                            row_bg = Some(rgba_a(color, ba));
                            border_col = rgba_a(color, bo);
                        }
                        glyph = Some((fg.kind.glyph(), gcolor));
                    } else if executed {
                        row_bg = Some(rgba_a(theme.accent, 0.04));
                    }
                    if error_line == Some(line_no as u32) {
                        row_bg = Some(rgba_a(theme.red, 0.12));
                        border_col = rgba_a(theme.red, 0.5);
                        glyph = Some((GlyphKind::Error.glyph(), rgba_a(theme.red, 1.0)));
                        nav_target = None;
                    }
                }

                // Gutter diagnostic dot (max severity starting on this row).
                let diag_dot = diag_dot_color(&tab.diagnostics, i, &theme);
                // Breakpoint on this line (§4.6) — takes visual precedence:
                // the whole row gets a red wash + left border.
                let has_bp = this.is_breakpoint(line_no as u32);
                if has_bp {
                    row_bg = Some(rgba_a(theme.red, 0.10));
                    border_col = rgba_a(theme.red, 0.45);
                }

                // `.w_full()`: uniform_list items size to content by default, so
                // short lines produced narrow rows and clicks to the right of
                // the code hit nothing (caret wouldn't move). Full-width rows
                // make the whole line band clickable (cell clamps to EOL).
                let mut row = div().flex().flex_row().w_full().h(px(LINE_H)).items_center();

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

                // (Structure-based whole-block row tints removed — deprecated.)
                row = row.border_l_2().border_color(border_col);
                if let Some(bg) = row_bg {
                    row = row.bg(bg);
                }

                // Gutter: diagnostic dot + right-aligned line number.
                let mut gutter = div()
                    .w(px(GUTTER_W))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .pr(px(8.));
                // The 8px glyph margin: click toggles a breakpoint (§4.6); it
                // shows a red breakpoint dot (precedence) or the diagnostic dot.
                let dot: gpui::AnyElement = if has_bp {
                    div()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded_full()
                        .bg(rgb(theme.red))
                        .into_any_element()
                } else {
                    match diag_dot {
                        Some(c) => div()
                            .w(px(5.))
                            .h(px(5.))
                            .rounded_full()
                            .bg(rgb(c))
                            .into_any_element(),
                        None => div().into_any_element(),
                    }
                };
                gutter = gutter.child(
                    div()
                        .id(("bp-margin", i))
                        .w(px(8.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |app: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
                                app.toggle_breakpoint(line_no as u32);
                                app.save_ui_state();
                                cx.notify();
                            }),
                        )
                        .child(dot),
                );
                // Line-number gutter: click toggles a breakpoint on this line
                // (§4.6 — same action as the dot margin, bigger target).
                gutter = gutter.child(
                    div()
                        .id(("gutter-line", i))
                        .flex_1()
                        .text_right()
                        .text_color(rgb(theme.muted))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |app: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
                                app.toggle_breakpoint(line_no as u32);
                                app.save_ui_state();
                                cx.notify();
                            }),
                        )
                        .child(line_no.to_string()),
                );
                row = row.child(gutter);

                // Fold chevron (§ hierarchical collapse): down = foldable +
                // open, right = folded; click toggles. Empty for non-blocks.
                let foldable = tab.fold_map.contains_key(&i);
                let folded = foldable && tab.folds.contains(&i);
                let mut fold_cell = div()
                    .id(("fold", i))
                    .w(px(FOLD_W))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center();
                if foldable {
                    let chev = if folded { "chevron-right" } else { "chevron-down" };
                    let ccol = if folded { theme.accent } else { theme.muted };
                    fold_cell = fold_cell
                        .cursor_pointer()
                        .child(crate::assets::ui_icon(chev, 11., ccol))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |app: &mut JadeApp, _ev: &MouseDownEvent, _w, cx| {
                                app.toggle_fold(i);
                                cx.notify();
                            }),
                        );
                }
                row = row.child(fold_cell);

                // Code cell: relative so the caret bar can be absolutely placed.
                // Stateful (`.id`) from the start so the mouse handlers below keep
                // the element type consistent.
                let mut cell = div()
                    .id(("code-cell", i))
                    .debug_selector(|| format!("code-cell-{i}"))
                    .flex_1()
                    .relative()
                    .overflow_hidden()
                    .flex()
                    .flex_row()
                    .items_center();

                // Caret bar (2px accent) on the caret row — blinks at the
                // app-driven 530ms phase while focused; steady dim marker when
                // the editor doesn't own keyboard focus (so it stays findable).
                if caret_point.row == i && (!focused || this.caret_blink_show) {
                    cell = cell.child(
                        div()
                            .absolute()
                            .top_0()
                            .left(px(editor_view::col_to_px(caret_point.col, char_w)))
                            .w(px(2.))
                            .h(px(LINE_H))
                            .bg(rgba_a(theme.accent, if focused { 1.0 } else { 0.35 }))
                            .debug_selector(|| "editor-caret".into()),
                    );
                }

                cell = cell.child(
                    div()
                        .whitespace_nowrap()
                        .flex_none()
                        .text_color(rgb(default_color))
                        .child(styled),
                );
                if folded {
                    cell = cell.child(
                        div()
                            .flex_none()
                            .px_1()
                            .text_color(rgba_a(theme.muted, 0.7))
                            .child("…"),
                    );
                }
                // Ghost text run (§4.11): faded (~40% muted) at the caret row,
                // appended right after the line text (before the EOL annotation).
                if focused {
                    if let Some((grow, gtext)) = &ghost_render {
                        if *grow == i {
                            cell = cell.child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .text_color(rgba_a(theme.muted, 0.4))
                                    .child(gtext.clone()),
                            );
                        }
                    }
                }
                if let Some(ann) = annotation {
                    cell = cell.child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .text_size(px(ANN_PX))
                            .text_color(rgba_a(theme.accent, 0.7))
                            .child(ann),
                    );
                }

                // Mouse: click sets caret / word / line; drag selects; ⌘-click →
                // definition; plain move → hover dwell.
                let mh = row_handle.clone();
                cell = cell
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |app: &mut JadeApp, ev: &MouseDownEvent, window, cx| {
                            if let Some(h) = &mh {
                                window.focus(h, cx);
                            }
                            let x = f32::from(ev.position.x);
                            if ev.modifiers.platform || ev.modifiers.control {
                                app.editor_goto_definition(i, x);
                            } else {
                                app.editor_mouse_down(i, x, ev.modifiers.shift, ev.click_count);
                            }
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(
                        move |app: &mut JadeApp, ev: &MouseMoveEvent, _w, cx| {
                            let x = f32::from(ev.position.x);
                            if ev.pressed_button == Some(MouseButton::Left) {
                                app.editor_mouse_drag(i, x);
                                cx.notify();
                            } else {
                                app.editor_hover_move(i, x);
                            }
                        },
                    ));

                row = row.child(cell);
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
    .font_family(crate::fonts::mono_family());

    let entity = cx.entity();
    let mut container = div()
        .relative()
        .flex()
        .flex_1()
        .size_full()
        .track_focus(&handle)
        .on_key_down(cx.listener(|app: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
            if app.editor_key(&ev.keystroke, cx) {
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|app: &mut JadeApp, _ev, _w, cx| {
                app.editor_mouse_up();
                cx.notify();
            }),
        )
        .child(list)
        .child(ime_geometry_canvas(
            flow_visible_now,
            app.editor_text_left.clone(),
            app.editor_rows.clone(),
            app.editor_char_w.clone(),
            handle.clone(),
            entity,
        ));

    let scroll_top = app.editor_scroll_top();
    if let Some(popup) = completion_popup(app, flow_visible_now, scroll_top, cx) {
        container = container.child(popup);
    }
    if let Some(popup) = hover_popup(app, flow_visible_now, scroll_top) {
        container = container.child(popup);
    }
    container.into_any_element()
}

/// Convert a flattened [`CellStyle`] into a gpui [`HighlightStyle`]: syntax color,
/// a 15%-alpha accent selection wash (§4.2), and a 1px diagnostic/IME underline
/// (a flat line — wavy squiggles are deferred; documented).
fn cell_to_style(cell: &CellStyle, theme: &Theme) -> HighlightStyle {
    let underline = cell
        .underline
        .map(|c| UnderlineStyle {
            thickness: px(1.0),
            color: Some(rgb(c).into()),
            wavy: false,
        })
        .or_else(|| {
            cell.marked.then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(rgb(theme.text).into()),
                wavy: false,
            })
        });
    HighlightStyle {
        color: cell.color.map(|c| rgb(c).into()),
        background_color: cell
            .selected
            .then(|| gpui::Hsla::from(rgba_a(theme.accent, 0.15))),
        underline,
        ..Default::default()
    }
}

/// The gutter dot color for the highest-severity diagnostic **starting** on row
/// `row` (0-based), or `None`.
fn diag_dot_color(diagnostics: &[forge_lsp::Diagnostic], row: usize, theme: &Theme) -> Option<u32> {
    use forge_lsp::DiagnosticSeverity as S;
    // Rank: lower is more severe (Error most). Track the most-severe on the row.
    let rank = |s: S| match s {
        S::ERROR => 0,
        S::WARNING => 1,
        S::INFORMATION => 2,
        _ => 3,
    };
    let mut best: Option<S> = None;
    for d in diagnostics {
        if d.range.start.line as usize == row {
            let sev = d.severity.unwrap_or(S::INFORMATION);
            best = Some(match best {
                Some(b) if rank(b) <= rank(sev) => b,
                _ => sev,
            });
        }
    }
    best.map(|s| editor_view::severity_color(Some(s), theme.red, theme.amber, theme.periwinkle))
}

/// The IME/geometry underlay: captures the code-text left edge + visible row
/// count for mouse mapping and follow-scroll, and registers the IME input
/// handler for the editor focus each frame (`window.handle_input`).
fn ime_geometry_canvas(
    flow_visible: bool,
    text_left: std::sync::Arc<std::sync::atomic::AtomicU32>,
    rows_store: std::sync::Arc<std::sync::atomic::AtomicU32>,
    char_w_store: std::sync::Arc<std::sync::atomic::AtomicU32>,
    handle: FocusHandle,
    entity: Entity<JadeApp>,
) -> impl IntoElement {
    use std::sync::atomic::Ordering;
    canvas(
        move |_bounds, _window, _cx| {},
        move |bounds: Bounds<gpui::Pixels>, _, window, cx| {
            let glyph = if flow_visible { GLYPH_W } else { 0.0 };
            let left = f32::from(bounds.origin.x) + 8.0 + GUTTER_W + FOLD_W + glyph;
            text_left.store(left.to_bits(), Ordering::Relaxed);
            // Measure the mono font's real advance so caret placement and
            // click→column mapping stay correct if the bundled font changes.
            let font = gpui::font(crate::fonts::mono_family());
            let font_id = window.text_system().resolve_font(&font);
            if let Ok(adv) = window.text_system().advance(font_id, px(FONT_PX), 'M') {
                let w = f32::from(adv.width);
                if w > 0.0 {
                    char_w_store.store(w.to_bits(), Ordering::Relaxed);
                }
            }
            let h = f32::from(bounds.size.height);
            rows_store.store(((h / LINE_H).floor() as u32).max(1), Ordering::Relaxed);
            let region = Bounds::new(
                point(px(left), bounds.origin.y + px(PAD_TOP)),
                size(bounds.size.width, bounds.size.height),
            );
            window.handle_input(&handle, ElementInputHandler::new(region, entity.clone()), cx);
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// The x pixel of the code column `col`, including the gutter + optional flow
/// margin, for anchoring the floating popups inside the editor container.
fn popup_x(col: usize, flow_visible: bool, char_w: f32) -> f32 {
    let glyph = if flow_visible { GLYPH_W } else { 0.0 };
    8.0 + GUTTER_W + FOLD_W + glyph + editor_view::col_to_px(col, char_w)
}

/// The y pixel (top) of the row below `row`, given the current scroll top.
fn popup_y(row: usize, scroll_top: usize) -> f32 {
    let rel = row.saturating_sub(scroll_top);
    PAD_TOP + (rel as f32 + 1.0) * LINE_H
}

/// The autocomplete popup (max ~8 visible), anchored under the caret.
fn completion_popup(
    app: &JadeApp,
    flow_visible: bool,
    scroll_top: usize,
    cx: &mut Context<JadeApp>,
) -> Option<gpui::AnyElement> {
    let c: &CompletionState = app.completion.as_ref()?;
    if c.filtered.is_empty() {
        return None;
    }
    let theme = app.theme.clone();
    let (row, col) = c.anchor;
    let row = app.editor.active_tab().map(|t| t.display_row(row)).unwrap_or(row);
    let left = popup_x(col, flow_visible, app.char_w());
    let top = popup_y(row, scroll_top);

    // Window of up to 8 items centered on the selection.
    let visible = 8usize;
    let start = c.selected.saturating_sub(visible - 1).min(
        c.filtered.len().saturating_sub(visible),
    );
    let mut list = div().flex().flex_col().py_1();
    for slot in start..(start + visible).min(c.filtered.len()) {
        let item_ix = c.filtered[slot];
        let item = &c.items[item_ix];
        let is_sel = slot == c.selected;
        let label = item.label.clone();
        let detail = item.detail.clone();
        let mut rowdiv = div()
            .id(("cmp", slot))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(20.))
            .px(px(8.))
            .gap_2()
            .text_xs()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |app: &mut JadeApp, _ev, _w, cx| {
                    if let Some(c) = &mut app.completion {
                        c.selected = slot;
                    }
                    app.completion_accept(cx);
                    cx.notify();
                }),
            )
            .child(div().text_color(rgb(theme.text)).child(label));
        if let Some(d) = detail {
            rowdiv = rowdiv.child(div().text_color(rgb(theme.muted)).child(d));
        }
        if is_sel {
            rowdiv = rowdiv.bg(rgb(theme.border));
        }
        list = list.child(rowdiv);
    }

    Some(
        div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .min_w(px(220.))
            .max_w(px(420.))
            .max_h(px(8.0 * 20.0 + 8.0))
            .flex()
            .flex_col()
            .rounded_md()
            .bg(rgb(theme.panel))
            .border_1()
            .border_color(rgb(theme.border))
            .overflow_hidden()
            .child(list)
            .into_any_element(),
    )
}

/// The floating hover panel (plain text; markdown rendering deferred).
fn hover_popup(app: &JadeApp, flow_visible: bool, scroll_top: usize) -> Option<gpui::AnyElement> {
    let h: &HoverState = app.hover.as_ref()?;
    let theme = app.theme.clone();
    let row = app.editor.active_tab().map(|t| t.display_row(h.row)).unwrap_or(h.row);
    let left = popup_x(h.col, flow_visible, app.char_w());
    let top = popup_y(row, scroll_top);
    Some(
        div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .max_w(px(480.))
            .max_h(px(220.))
            .p(px(8.))
            .rounded_md()
            .bg(rgb(theme.panel))
            .border_1()
            .border_color(rgb(theme.border))
            .overflow_hidden()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme.text))
                    .whitespace_normal()
                    .child(h.text.clone()),
            )
            .into_any_element(),
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
/// an `x` close icon, the active tab underlined. Middle-click also closes (GPUI exposes
/// the mouse button on the down event).
pub fn tab_strip(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    // Seamless strip (screenshot ref): darker band, active tab shares the
    // editor's background and carries the accent underline — no chip boxes.
    let mut strip = div()
        .id("tab-strip")
        .flex()
        .flex_row()
        .items_end()
        .h(px(34.))
        .w_full()
        .px(px(6.))
        .bg(rgb(theme.panel))
        .overflow_x_hidden();

    for (i, tab) in app.editor.tabs.iter().enumerate() {
        let active = app.editor.active == Some(i);
        strip = strip.child(tab_chip(i, &tab.name, active, tab.buffer.is_dirty(), theme, cx));
    }
    // XP bar in the tab-bar right slot (§4.10): flex spacer pushes it to the edge.
    strip
        .child(div().flex_1())
        .child(xp_bar(app, theme))
}

/// The XP bar (§4.10): `L{n}` label · progress track · `×{streak}` badge (shown
/// only when the streak is live). Mounted in the tab-bar right slot.
fn xp_bar(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let info = crate::xp::level_from_xp(app.xp.total());
    let streak = app.xp.effective_streak(app.now_ms());
    let pct = if info.needed > 0 {
        (info.progress as f32 / info.needed as f32 * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let mut bar = div()
        .id("xp-bar")
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px(px(6.))
        // Tooltip parity with the TS: progress / needed to next level · total.
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme.accent))
                .child(format!("L{}", info.level)),
        )
        .child(
            // Progress track + fill.
            div()
                .w(px(60.))
                .h(px(6.))
                .rounded_full()
                .bg(rgb(theme.border))
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(pct / 100.0))
                        .bg(rgb(theme.accent)),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme.muted))
                .child(format!("{}/{}", info.progress, info.needed)),
        );
    if streak > 1 {
        bar = bar.child(
            div()
                .text_xs()
                .text_color(rgb(theme.amber))
                .child(format!("×{streak}")),
        );
    }
    bar
}

fn tab_chip(
    index: usize,
    name: &str,
    active: bool,
    dirty: bool,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let fg = if active { theme.text } else { theme.muted };
    let hover_bg = theme.border;

    // Seamless tab: full strip height, the active tab takes the editor's own
    // background (merging into the code below) + a 2px accent underline;
    // inactive tabs are flat text on the strip band.
    let mut chip = div()
        .id(("tab", index))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(30.))
        .px(px(14.))
        .rounded_t_md()
        .text_xs()
        .cursor_pointer()
        .text_color(rgb(fg))
        // Middle-click closes (mouse-down carries the button).
        .on_mouse_down(
            gpui::MouseButton::Middle,
            cx.listener(move |app, _ev, _win, cx| {
                app.close_tab(index);
                app.schedule_ui_save(cx);
                cx.notify();
            }),
        )
        // Left-click switches to this tab.
        .on_click(cx.listener(move |app, _ev, _win, cx| {
            app.switch_tab(index);
            app.schedule_ui_save(cx);
            cx.notify();
        }))
        .child(div().child(name.to_string()));

    if active {
        chip = chip.bg(rgb(theme.bg)).border_b_2().border_color(rgb(theme.accent));
    } else {
        chip = chip.hover(move |st| st.bg(rgb(hover_bg)));
    }

    // Dirty dot ● from buffer.is_dirty() (§3); silent-close semantics preserved.
    if dirty {
        chip = chip.child(div().text_color(rgb(theme.accent)).child("●"));
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
                app.schedule_ui_save(cx);
                cx.notify();
            }))
            .child(crate::assets::ui_icon("x", 12., theme.muted)),
    )
}
