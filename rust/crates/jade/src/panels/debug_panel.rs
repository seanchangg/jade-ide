//! Debug panel (feature inventory §5.8).
//!
//! CLion-style panel that docks above the terminal (hiding it, restored on hide —
//! the docking lives in `app.rs`). Header: a status line (running muted / paused
//! amber bold / exited italic) + Continue · Step Over/Into/Out · Stop · close.
//! Columns: FRAMES (240px, click navigates, frame 0 active) | VARIABLES
//! (expandable tree, lazy child fetch, expansion survives steps) | CONSOLE
//! (ANSI-stripped, autoscroll). A pure projection over [`crate::debug::DebugSession`].

use gpui::{div, prelude::*, px, rgb, Context, FontWeight, SharedString};

use crate::app::JadeApp;
use crate::debug::{flatten_vars, DebugStatus};
use crate::theme::Theme;

/// Panel height (§5.8 "240px").
const PANEL_H: f32 = 240.0;

pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = app.theme.clone();

    div()
        .id("debug-panel")
        .flex()
        .flex_col()
        .h(px(PANEL_H))
        .w_full()
        .bg(rgb(theme.bg))
        .border_t_1()
        .border_color(rgb(theme.border))
        .child(header(app, &theme, cx))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .w_full()
                .overflow_hidden()
                .child(frames_col(app, &theme, cx))
                .child(variables_col(app, &theme, cx))
                .child(console_col(app, &theme)),
        )
}

fn header(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> impl IntoElement {
    // Status line with §5.8 colors.
    let (status_text, status_color, bold, italic) = match app.debug.status {
        DebugStatus::Running => ("running".to_string(), theme.muted, false, false),
        DebugStatus::Paused => {
            let loc = app
                .debug
                .location
                .as_ref()
                .map(|(f, l)| format!("paused · {}:{}", short(f), l))
                .unwrap_or_else(|| "paused".to_string());
            (loc, theme.amber, true, false)
        }
        DebugStatus::Exited => {
            let code = app.debug.exit_code.unwrap_or(0);
            (format!("exited ({code})"), theme.muted, false, true)
        }
        DebugStatus::Idle => ("idle".to_string(), theme.muted, false, false),
    };

    let mut status = div().text_xs().text_color(rgb(status_color)).child(status_text);
    if bold {
        status = status.font_weight(FontWeight::BOLD);
    }
    if italic {
        status = status.italic();
    }

    // Icon-only control button; `icon` is a bundled lucide glyph name.
    let btn = |id: &'static str,
               icon: &'static str,
               action: fn(&mut JadeApp, &mut Context<JadeApp>),
               cx: &mut Context<JadeApp>| {
        div()
            .id(id)
            .flex()
            .items_center()
            .px_2()
            .py(px(2.))
            .rounded_md()
            .text_xs()
            .bg(rgb(theme.panel))
            .text_color(rgb(theme.text))
            .cursor_pointer()
            .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                action(a, cx);
                cx.notify();
            }))
            .child(crate::assets::ui_icon(icon, 13., theme.text))
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(26.))
        .px(px(8.))
        .bg(rgb(theme.panel))
        .border_b_1()
        .border_color(rgb(theme.border))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .text_color(rgb(theme.accent))
                        .child(crate::assets::ui_icon("bug", 13., theme.accent)),
                )
                .child(div().text_color(rgb(theme.accent)).text_xs().child("DEBUG"))
                .child(status),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(btn("dbg-cont", "play", |a, _| a.debug_continue(), cx))
                .child(btn("dbg-over", "arrow-down", |a, _| a.debug_step_over(), cx))
                .child(btn("dbg-into", "arrow-down-to-line", |a, _| a.debug_step_into(), cx))
                .child(btn("dbg-out", "arrow-up-from-line", |a, _| a.debug_step_out(), cx))
                .child(btn("dbg-stop", "square", |a, _| a.action_stop(), cx))
                .child(
                    div()
                        .id("dbg-close")
                        .flex()
                        .items_center()
                        .px_1()
                        .text_color(rgb(theme.muted))
                        .cursor_pointer()
                        .on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
                            a.hide_debug();
                            cx.notify();
                        }))
                        .child(crate::assets::ui_icon("x", 12., theme.muted)),
                ),
        )
}

fn col_label(text: &str, theme: &Theme) -> impl IntoElement {
    div()
        .text_color(rgb(theme.muted))
        .text_size(px(9.))
        .px(px(6.))
        .py(px(2.))
        .child(text.to_string())
}

fn frames_col(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let mut col = div()
        .id("dbg-frames")
        .flex()
        .flex_col()
        .w(px(240.))
        .flex_none()
        .h_full()
        .overflow_y_scroll()
        .border_r_1()
        .border_color(rgb(theme.border))
        .child(col_label("FRAMES", theme));

    for (i, f) in app.debug.frames.iter().enumerate() {
        let active = i == app.debug.active_frame;
        let mut row = div()
            .id(("frame", i))
            .flex()
            .flex_col()
            .px(px(6.))
            .py(px(1.))
            .cursor_pointer()
            .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                a.debug_select_frame(i);
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(if active { theme.accent } else { theme.text }))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(f.function_name.clone()),
            )
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(theme.muted))
                    .child(format!("{}:{}", short(&f.file), f.line)),
            );
        if active {
            row = row.bg(rgb(theme.panel));
        }
        col = col.child(row);
    }
    col
}

fn variables_col(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let mut col = div()
        .id("dbg-vars")
        .flex()
        .flex_col()
        .flex_1() // ~1.2 share vs the console
        .h_full()
        .overflow_y_scroll()
        .border_r_1()
        .border_color(rgb(theme.border))
        .child(col_label("VARIABLES", theme));

    for row in flatten_vars(&app.debug) {
        let indent = 6.0 + row.depth as f32 * 12.0;
        let marker = if row.expandable {
            if row.expanded {
                "▾"
            } else {
                "▸"
            }
        } else {
            " "
        };
        let path = row.var.path.clone();
        let expandable = row.expandable;
        let line = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .pl(px(indent))
            .pr(px(6.))
            .h(px(18.))
            .text_size(px(11.))
            .whitespace_nowrap();

        // Expansion caret (only clickable when expandable + has a path).
        let caret: gpui::AnyElement = match (expandable, path.clone()) {
            (true, Some(p)) => div()
                .id(SharedString::from(format!("var-{p}")))
                .w(px(10.))
                .flex_none()
                .text_color(rgb(theme.muted))
                .cursor_pointer()
                .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                    a.debug_toggle_var(&p);
                    cx.notify();
                }))
                .child(marker)
                .into_any_element(),
            _ => div()
                .w(px(10.))
                .flex_none()
                .text_color(rgb(theme.muted))
                .child(marker)
                .into_any_element(),
        };
        let line = line
            .child(caret)
            .child(div().text_color(rgb(theme.periwinkle)).child(row.var.name.clone()))
            .child(div().text_color(rgb(theme.muted)).child(":"))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_color(rgb(theme.text))
                    .child(row.var.value.clone()),
            )
            .child(div().text_color(rgb(theme.muted)).child(row.var.type_.clone()));
        col = col.child(line);
    }
    col
}

fn console_col(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let start = app.debug.console.len().saturating_sub(300);
    let mut list = div().flex().flex_col();
    for line in &app.debug.console[start..] {
        let text = if line.is_empty() { " ".to_string() } else { line.clone() };
        list = list.child(
            div()
                .text_size(px(11.))
                .text_color(rgb(theme.muted))
                .whitespace_nowrap()
                .child(text),
        );
    }
    div()
        .id("debug-console")
        .flex()
        .flex_col()
        .flex_1()
        .h_full()
        .overflow_y_scroll()
        .child(col_label("CONSOLE", theme))
        .child(div().px(px(6.)).font_family(crate::fonts::mono_family()).child(list))
}

/// Shorten a path to its basename for the compact frame/status readouts.
fn short(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
