//! Read-only code viewer (deliverable §5).
//!
//! A virtualized line list (`uniform_list`) so a 5k-line file renders only the
//! visible rows, never 5k divs/frame. Each row is a fixed-height (20px) line: a
//! right-aligned gutter line number (#4B4E56) plus the code, rendered as a single
//! [`StyledText`] whose per-token color runs come from the tab's precomputed
//! highlight spans (`with_highlights`; gaps fall back to the default text color).
//! Menlo 13px, line-height 20, padding-top 16, horizontal overflow clipped
//! (no wrap).

use gpui::{div, prelude::*, px, rgb, uniform_list, Context, HighlightStyle};

use crate::app::JadeApp;
use crate::theme::Theme;

/// Editor metrics (§4.1 "editor look").
const FONT_PX: f32 = 13.0;
const LINE_H: f32 = 20.0;
const PAD_TOP: f32 = 16.0;
const GUTTER_W: f32 = 52.0;

/// Gutter line-number colors (§4.1).
const GUTTER_FG: u32 = 0x4B4E56;

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
            cx.processor(move |this, range: std::ops::Range<usize>, _window, _cx| {
                let mut rows = Vec::with_capacity(range.len());
                let Some(tab) = this.editor.active_tab() else {
                    return rows;
                };
                for i in range {
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

                    let line_no = i + 1;
                    let styled = gpui::StyledText::new(text).with_highlights(highlights);

                    rows.push(
                        div()
                            .flex()
                            .flex_row()
                            .h(px(LINE_H))
                            .items_center()
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
                            .child(
                                // Code cell: never wraps; overflow clipped by list.
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_color(rgb(default_color))
                                    .child(styled),
                            ),
                    );
                }
                rows
            }),
        )
        .flex_1()
        .size_full()
        .pt(px(PAD_TOP))
        .px(px(8.))
        .text_size(px(FONT_PX))
        .line_height(px(LINE_H))
        .font_family("Menlo"),
    )
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
