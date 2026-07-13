//! TELEMETRY sidebar (feature inventory §5.6): SCALARS / TIMERS / BUFFERS
//! sections of checkbox rows with live value readouts. Toggling a checkbox
//! flips registry state, persists the preference, and sends `track` to the
//! probe (all via `JadeApp::toggle_enabled`).
//!
//! The inline rows×cols shape editor from the TS panel is deferred to Phase-3
//! (it needs a text-input widget); buffers show a static shape glyph as its
//! placeholder, and the shape/maxdim preferences are already persisted so the
//! editor can be wired without a format change.

use gpui::{div, prelude::*, px, rgb, Context, SharedString};

use forge_telemetry::Kind;

use crate::app::JadeApp;
use crate::format::{format_buffer_value, format_scalar_value, format_timer_value};
use crate::registry::{key_of, RegistryItem};
use crate::theme::Theme;

pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = &app.theme;

    let mut col = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_color(rgb(theme.accent))
                .text_xs()
                .child("TELEMETRY"),
        );

    for (kind, title) in [
        (Kind::Scalar, "SCALARS"),
        (Kind::Timer, "TIMERS"),
        (Kind::Buffer, "BUFFERS"),
    ] {
        col = col.child(section(app, cx, kind, title));
    }

    col
}

fn section(
    app: &JadeApp,
    cx: &mut Context<JadeApp>,
    kind: Kind,
    title: &str,
) -> impl IntoElement {
    let theme = &app.theme;
    let items = app.registry.items_of_kind(kind);

    let mut body = div().flex().flex_col().gap_1();
    if items.is_empty() {
        body = body.child(
            div()
                .text_color(rgb(theme.muted))
                .text_size(px(10.))
                .child("none discovered"),
        );
    } else {
        for item in items {
            body = body.child(row(cx, theme, kind, item));
        }
    }

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_color(rgb(theme.muted))
                .text_size(px(9.))
                .child(title.to_string()),
        )
        .child(body)
}

fn row(
    cx: &mut Context<JadeApp>,
    theme: &Theme,
    kind: Kind,
    item: &RegistryItem,
) -> impl IntoElement {
    let name = item.name.clone();
    let enabled = item.enabled;
    let key = key_of(kind, &item.name);

    // Checkbox: a small box, filled when enabled.
    let mut checkbox = div()
        .w(px(12.))
        .h(px(12.))
        .rounded_sm()
        .border_1()
        .border_color(rgb(theme.accent));
    if enabled {
        checkbox = checkbox.bg(rgb(theme.accent));
    }

    let value = value_readout(kind, item);

    let mut row = div()
        .id(SharedString::from(format!("telrow-{}", key)))
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(11.))
        .cursor_pointer()
        .on_click(cx.listener(move |app: &mut JadeApp, _ev, _win, cx| {
            app.toggle_enabled(kind, &name);
            cx.notify();
        }))
        .child(checkbox)
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .text_color(rgb(theme.text))
                .child(item.name.clone()),
        )
        .child(
            div()
                .text_color(rgb(theme.muted))
                .child(value),
        );

    // Buffers: static shape-editor placeholder (see module doc).
    if kind == Kind::Buffer {
        row = row.child(
            div()
                .text_color(rgb(theme.muted))
                .text_size(px(10.))
                .child("⊞"),
        );
    }

    row
}

fn value_readout(kind: Kind, item: &RegistryItem) -> String {
    match kind {
        Kind::Buffer => {
            let r = item.last_rows.or(item.meta_rows);
            let c = item.last_cols.or(item.meta_cols);
            format_buffer_value(r, c, item.last_step)
        }
        Kind::Timer => item
            .last_value
            .map(format_timer_value)
            .unwrap_or_else(|| "—".to_string()),
        Kind::Scalar => item
            .last_value
            .map(format_scalar_value)
            .unwrap_or_else(|| "—".to_string()),
    }
}
