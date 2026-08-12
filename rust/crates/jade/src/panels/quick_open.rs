//! Quick Open overlay (feature inventory §5.7).
//!
//! A ⌘P overlay centered in the editor area. The input is **not** a full text
//! widget — it is a captured-keystroke buffer: the overlay div takes focus and
//! its `on_key_down` forwards each keystroke to `JadeApp::quick_open_key`, which
//! drives the pure [`crate::quick_open::QuickOpenState`] (printable chars append,
//! Backspace pops, ↑/↓ move the selection, Enter opens, Esc closes). ⌘P itself is
//! left unhandled here so it bubbles to the global toggle on the app root.
//!
//! The matcher, the 10-result cap, and the relative-path-hint rule all live in
//! the pure `crate::quick_open` module; this file only paints.

use gpui::{div, prelude::*, px, rgb, Context, FocusHandle, KeyDownEvent, SharedString};

use crate::app::JadeApp;

/// True when a keystroke should type a character: a printable `key_char` with no
/// command/control/alt modifier (so ⌘P / ⌃C don't insert text).
fn is_printable(ev: &KeyDownEvent) -> bool {
    let m = &ev.keystroke.modifiers;
    ev.keystroke.key_char.is_some() && !m.platform && !m.control && !m.alt && !m.function
}

/// Build the overlay element. `focus` is the pre-created handle (focused by the
/// app on show so keystrokes land here).
pub fn overlay(app: &JadeApp, focus: FocusHandle, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    let theme = app.theme.clone();
    let state = app.quick_open.as_ref();
    let query = state.map(|s| s.query.clone()).unwrap_or_default();
    let selected = state.map(|s| s.selected).unwrap_or(0);
    let results = app.quick_open_matches();

    // Input row: the captured query buffer + caret (before the placeholder
    // when empty — shared fake-input rule).
    let input = div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(34.))
        .px(px(10.))
        .border_b_1()
        .border_color(rgb(theme.border))
        .child(crate::panels::input_line(
            &query,
            "Search files by name…",
            true,
            true,
            theme.accent,
            &theme,
            query.len(),
        ));

    // Results list.
    let mut list = div().flex().flex_col().py_1();
    if results.is_empty() {
        list = list.child(
            div()
                .px(px(10.))
                .py_1()
                .text_color(rgb(theme.muted))
                .text_xs()
                .child("No matching files"),
        );
    } else {
        for (idx, m) in results.iter().enumerate() {
            let is_sel = idx == selected;
            let path = m.path.clone();
            let mut row = div()
                .id(SharedString::from(format!("qo-{idx}")))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .h(px(24.))
                .px(px(10.))
                .text_xs()
                .cursor_pointer()
                .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                    a.quick_open_open(path.clone());
                    cx.notify();
                }))
                .child(div().text_color(rgb(theme.text)).child(m.name.clone()));
            if is_sel {
                row = row.bg(rgb(theme.border));
            }
            if let Some(hint) = &m.hint {
                row = row.child(
                    div()
                        .text_color(rgb(theme.muted))
                        .child(SharedString::from(hint.clone())),
                );
            }
            list = list.child(row);
        }
    }

    let panel = div()
        .w(px(520.))
        .max_h(px(360.))
        .flex()
        .flex_col()
        .rounded_lg()
        .bg(rgb(theme.panel))
        .border_1()
        .border_color(rgb(theme.border))
        .overflow_hidden()
        .child(input)
        .child(
            div()
                .id("qo-results")
                .flex_1()
                .overflow_y_scroll()
                .child(list),
        );

    // Full-area overlay: dim backdrop (click closes), panel centered near the top.
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .pt(px(80.))
        .track_focus(&focus)
        .on_key_down(cx.listener(|a: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
            a.quick_open_key(
                &ev.keystroke.key.clone(),
                ev.keystroke.key_char.clone(),
                is_printable(ev),
            );
            cx.notify();
        }))
        // Backdrop click-catcher (behind the panel) closes the overlay.
        .child(
            div()
                .id("qo-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
                    a.close_quick_open();
                    cx.notify();
                })),
        )
        .child(panel)
        .into_any_element()
}
