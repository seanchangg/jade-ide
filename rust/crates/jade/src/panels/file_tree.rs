//! File-tree panel (feature inventory §5.1, deliverable §2).
//!
//! Header "FILES" + a minimize placeholder; one row per visible tree node with an
//! expand arrow (▸/▾) for directories, the extension glyph map from §5.1
//! (`◆ ◇ ◈ {} ≡ ¶ ◎ $ ⚙ ▣ ·`), indentation `12 + depth*16` px, source files
//! accent-colored / headers accent2, and the active file's row highlighted.
//! Clicking a directory toggles expansion (lazy-loading its children); clicking a
//! file opens it in the editor.
//!
//! Rows are plain divs (not virtualized): the visible set is bounded by what the
//! user has expanded, and per-row click handlers use `cx.listener`. The heavy
//! virtualization lives in the code viewer (5k-line files), not here.

use gpui::{div, prelude::*, px, rgb, Context};

use crate::app::JadeApp;
use crate::theme::Theme;
use crate::workspace_tree::{ExtClass, Row};

pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = app.theme.clone();

    // Header: FILES + a (non-functional) minimize placeholder.
    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(div().text_color(rgb(theme.muted)).text_xs().child("FILES"))
        .child(
            div()
                .id("files-minimize")
                .text_color(rgb(theme.muted))
                .text_xs()
                .child("—"),
        );

    let mut list = div().flex().flex_col().w_full();

    match &app.tree {
        Some(tree) => {
            let active = app.active_file.clone();
            for row in tree.visible_rows() {
                list = list.child(tree_row(row, active.as_deref(), &theme, cx));
            }
        }
        None => {
            list = list.child(
                div()
                    .text_color(rgb(theme.muted))
                    .text_xs()
                    .child("Open a folder to get started"),
            );
        }
    }

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(header)
        .child(div().id("file-tree-scroll").flex_1().overflow_y_scroll().child(list))
}

fn tree_row(
    row: Row,
    active: Option<&std::path::Path>,
    theme: &Theme,
    cx: &mut Context<JadeApp>,
) -> impl IntoElement {
    let indent = 12.0 + row.depth as f32 * 16.0;
    let is_active = active == Some(row.path.as_path());
    let path = row.path.clone();

    // Leading glyph: expand arrow for dirs, extension glyph for files.
    let (glyph, glyph_color) = if row.is_dir {
        let arrow = if row.expanded { "▾" } else { "▸" };
        (arrow.to_string(), theme.muted)
    } else {
        let color = match row.ext_class {
            ExtClass::Source => theme.accent,
            ExtClass::Header => theme.blue_gray,
            ExtClass::Other => theme.muted,
        };
        (file_glyph(&row.name).to_string(), color)
    };

    // Label color: source accent, header accent2, dirs/others default text.
    let label_color = if is_active {
        theme.accent
    } else {
        match (row.is_dir, row.ext_class) {
            (false, ExtClass::Source) => theme.accent,
            (false, ExtClass::Header) => theme.blue_gray,
            _ => theme.text,
        }
    };

    let mut el = div()
        .id(("tree-row", row_id(&row.path)))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(22.))
        .pl(px(indent))
        .pr(px(6.))
        .rounded_sm()
        .text_xs()
        .cursor_pointer();

    if is_active {
        el = el.bg(rgb(theme.border));
    }

    let is_dir = row.is_dir;
    el.on_click(cx.listener(move |app, _ev, _win, cx| {
        if is_dir {
            app.toggle_dir(path.clone());
        } else {
            app.open_file(path.clone());
        }
        cx.notify();
    }))
    .child(
        div()
            .w(px(14.))
            .flex_none()
            .text_color(rgb(glyph_color))
            .child(glyph),
    )
    .child(div().text_color(rgb(label_color)).child(row.name))
}

/// Stable-ish element id from a path (hash of the string form).
fn row_id(path: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

/// Extension → glyph, mirroring `file-tree.ts:504-522`. Files with no dot use the
/// whole (lowercased) name as the "extension" (so `Makefile` → ⚙).
fn file_glyph(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
    match ext.as_str() {
        "cpp" | "cc" | "cxx" => "◆",
        "c" => "◇",
        "h" | "hpp" | "hxx" => "◈",
        "json" => "{}",
        "txt" => "≡",
        "md" => "¶",
        "py" => "◎",
        "sh" | "zsh" => "$",
        "makefile" | "cmake" => "⚙",
        "cu" | "metal" => "▣",
        _ => "·",
    }
}
