//! Sticky-note overlay (§4.9). Absolute cards floating over the whole window at
//! their stored `(x, y)` — they **deliberately do not scroll with the editor**.
//! Only the active file's notes are shown. Interaction:
//!   - header drag → move (persist on mouseup),
//!   - corner handle → resize (min 160×100, persist on mouseup),
//!   - `×` → delete,
//!   - body click → focus for editing (captured-keystroke buffer, §4.9),
//!   - checkbox click → toggle `[ ]`/`[x]`.
//!
//! The model/persistence/checkbox logic is the pure `crate::notes` module; this
//! file only paints + wires mouse/key events onto `JadeApp` methods.
//!
//! ## Text-editing limits (documented)
//! Editing is an append/backspace buffer (like Quick Open), not a full caret
//! widget: printable keys append, Backspace pops the last char, Enter inserts a
//! newline, Esc blurs. `[ ]`/`[x]` render live as `☐`/`☑`; the stored content
//! stays markdown so it round-trips with the Electron app.

use gpui::{
    div, prelude::*, px, rgb, Context, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, SharedString,
};

use crate::app::JadeApp;
use crate::notes::Segment;

/// Build the sticky-note overlay for the active file, or `None` when there are no
/// notes to show and no drag in flight (so the editor stays fully interactive).
pub fn overlay(app: &JadeApp, focus: FocusHandle, cx: &mut Context<JadeApp>) -> Option<gpui::AnyElement> {
    let notes = app.active_notes();
    if notes.is_empty() && !app.note_pointer_active() {
        return None;
    }
    let theme = app.theme.clone();

    // Pass-through container: no id/handlers, so empty areas don't block the
    // editor beneath (only the interactive cards + capture layer catch events).
    let mut root = div().absolute().top_0().left_0().size_full();

    // Drag/resize capture layer — only present (and interactive) while a drag or
    // resize is in flight, so it never blocks the editor otherwise.
    if app.note_pointer_active() {
        root = root.child(
            div()
                .id("note-capture")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_mouse_move(cx.listener(|a: &mut JadeApp, ev: &MouseMoveEvent, _w, cx| {
                    a.note_pointer_move(f32::from(ev.position.x), f32::from(ev.position.y));
                    cx.notify();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|a: &mut JadeApp, _ev, _w, cx| {
                        a.note_pointer_up();
                        cx.notify();
                    }),
                ),
        );
    }

    for note in &notes {
        root = root.child(card(app, note, &theme, &focus, cx));
    }
    Some(root.into_any_element())
}

fn card(
    app: &JadeApp,
    note: &crate::notes::StickyNote,
    theme: &crate::theme::Theme,
    focus: &FocusHandle,
    cx: &mut Context<JadeApp>,
) -> gpui::AnyElement {
    let editing = app.note_editing.as_deref() == Some(note.id.as_str());
    let id = note.id.clone();

    // Header: L{line} tag · close ×. Dragging the header moves the note.
    let hid = id.clone();
    let did = id.clone();
    let header = div()
        .id(SharedString::from(format!("note-hdr-{}", note.id)))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(20.))
        .px(px(6.))
        .bg(rgb(theme.panel))
        .border_b_1()
        .border_color(rgb(theme.border))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |a: &mut JadeApp, ev: &MouseDownEvent, _w, cx| {
                a.note_drag_start(&hid, f32::from(ev.position.x), f32::from(ev.position.y));
                cx.notify();
            }),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme.accent))
                .child(format!("L{}", note.anchor_line)),
        )
        .child(
            div()
                .id(SharedString::from(format!("note-x-{}", note.id)))
                .px_1()
                .text_color(rgb(theme.muted))
                .cursor_pointer()
                .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                    a.delete_note(&did);
                    cx.notify();
                }))
                .child("×"),
        );

    // Content body: rendered lines with clickable checkboxes.
    let bid = id.clone();
    let mut body = div()
        .id(SharedString::from(format!("note-body-{}", note.id)))
        .flex()
        .flex_1()
        .flex_col()
        .p(px(6.))
        .text_xs()
        .overflow_hidden()
        .cursor_text()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |a: &mut JadeApp, _ev, _w, cx| {
                a.edit_note(&bid);
                cx.notify();
            }),
        );

    if note.content.is_empty() {
        body = body.child(
            div()
                .text_color(rgb(theme.muted))
                .child(if editing { "" } else { "Click to add a note…" }),
        );
    } else {
        for (line_idx, segs) in crate::notes::render_segments(&note.content).into_iter().enumerate() {
            let mut line = div().flex().flex_row().items_center().min_h(px(16.));
            if segs.is_empty() {
                line = line.child(div().child(" "));
            }
            for (seg_idx, seg) in segs.into_iter().enumerate() {
                match seg {
                    Segment::Text(t) => {
                        line = line.child(div().text_color(rgb(theme.text)).child(t));
                    }
                    Segment::Checkbox { checked, index } => {
                        let nid = id.clone();
                        line = line.child(
                            div()
                                .id(SharedString::from(format!("note-cb-{}-{line_idx}-{seg_idx}", note.id)))
                                .px_1()
                                .cursor_pointer()
                                .text_color(rgb(if checked { theme.accent } else { theme.muted }))
                                .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                                    a.toggle_note_checkbox(&nid, index);
                                    cx.notify();
                                }))
                                .child(if checked { "☑" } else { "☐" }),
                        );
                    }
                }
            }
            body = body.child(line);
        }
    }

    // Resize handle (bottom-right corner).
    let rid = id.clone();
    let resize = div()
        .id(SharedString::from(format!("note-rz-{}", note.id)))
        .absolute()
        .bottom_0()
        .right_0()
        .w(px(12.))
        .h(px(12.))
        .cursor_pointer()
        .text_color(rgb(theme.muted))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |a: &mut JadeApp, ev: &MouseDownEvent, _w, cx| {
                a.note_resize_start(&rid, f32::from(ev.position.x), f32::from(ev.position.y));
                cx.notify();
            }),
        )
        .child("◢");

    let mut cardel = div()
        .id(SharedString::from(format!("note-card-{}", note.id)))
        .absolute()
        .left(px(note.x))
        .top(px(note.y))
        .w(px(note.width))
        .h(px(note.height))
        .flex()
        .flex_col()
        .rounded_md()
        .bg(rgb(theme.bg))
        .border_1()
        .border_color(rgb(if editing { theme.accent } else { theme.border }))
        .overflow_hidden()
        .child(header)
        .child(body)
        .child(resize);

    // Capture keystrokes only while this note is the one being edited.
    if editing {
        cardel = cardel
            .track_focus(focus)
            .on_key_down(cx.listener(|a: &mut JadeApp, ev: &KeyDownEvent, _w, cx| {
                if a.note_key(&ev.keystroke, cx) {
                    cx.stop_propagation();
                    cx.notify();
                }
            }));
    }

    cardel.into_any_element()
}
