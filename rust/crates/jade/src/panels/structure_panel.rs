//! STRUCTURE panel (feature inventory §5.5).
//!
//! The left sidebar switches between FILES (the tree) and STRUCTURE (this) via a
//! tab switcher at the top ([`tab_switcher`]). STRUCTURE renders the active tab's
//! tree-sitter outline ([`crate::structure`]) as a mermaid-style tree: nested
//! symbols sit inside a group container with a left border (the vertical
//! connector), each row is a kind-colored dot + name, and clicking a row reveals
//! its line in the code viewer (`JadeApp::reveal_line`). The symbols themselves
//! come from [`crate::structure::parse_symbols`], computed once per open tab.

use gpui::{div, prelude::*, px, rgb, Context, SharedString};

use crate::app::{JadeApp, SidebarTab};
use crate::structure::{kind_color, Symbol, SymbolKind};
use crate::theme::Theme;

/// FILES | STRUCTURE switcher shown at the top of the left sidebar. Clicking a
/// tab flips `JadeApp::sidebar_tab`.
pub fn tab_switcher(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    let tab = |id: &'static str,
               icon: &'static str,
               label: &'static str,
               which: SidebarTab,
               active: bool| {
        let color = if active { theme.text } else { theme.muted };
        let mut el = div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .text_xs()
            .cursor_pointer()
            .text_color(rgb(color))
            .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                a.set_sidebar_tab(which);
                cx.notify();
            }))
            .child(crate::assets::ui_icon(icon, 13.))
            .child(label);
        if active {
            el = el.border_b_2().border_color(rgb(theme.accent));
        }
        el
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(tab(
            "sb-files",
            "folder",
            "FILES",
            SidebarTab::Files,
            app.sidebar_tab == SidebarTab::Files,
        ))
        .child(tab(
            "sb-structure",
            "list-tree",
            "STRUCTURE",
            SidebarTab::Structure,
            app.sidebar_tab == SidebarTab::Structure,
        ))
}

/// Render the outline for the active tab (or a muted placeholder).
pub fn render(app: &JadeApp, cx: &mut Context<JadeApp>) -> impl IntoElement {
    let theme = app.theme.clone();
    let symbols = app.active_symbols();

    let mut list = div().flex().flex_col().w_full();
    if symbols.is_empty() {
        let msg = if app.editor.active_tab().is_some() {
            "No symbols"
        } else {
            "No file open"
        };
        list = list.child(div().text_color(rgb(theme.muted)).text_xs().child(msg));
    } else {
        for sym in symbols {
            list = list.child(node(sym, &theme, cx));
        }
    }

    div()
        .id("structure-scroll")
        .flex_1()
        .overflow_y_scroll()
        .child(list)
}

/// One symbol row plus (indented, connector-bordered) its children.
fn node(sym: &Symbol, theme: &Theme, cx: &mut Context<JadeApp>) -> gpui::AnyElement {
    let line = sym.line;
    let dot_color = kind_color(sym.kind, theme);

    // The clickable pill: dot + name (+ a muted access tag for members/methods).
    let mut pill = div()
        .id(SharedString::from(format!("sym-{}-{}", sym.name, sym.line)))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(20.))
        .px_1()
        .rounded_sm()
        .text_xs()
        .cursor_pointer()
        .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
            a.reveal_line(line);
            cx.notify();
        }))
        .child(
            // Kind-colored dot.
            div()
                .w(px(7.))
                .h(px(7.))
                .flex_none()
                .rounded_full()
                .bg(rgb(dot_color)),
        )
        .child(div().text_color(rgb(theme.text)).child(sym.name.clone()));

    if let Some(access) = sym.access {
        pill = pill.child(
            div()
                .text_color(rgb(theme.muted))
                .child(SharedString::from(access.label())),
        );
    }
    // A subtle kind label keeps free functions vs types legible.
    pill = pill.child(
        div()
            .text_color(rgb(theme.muted))
            .child(SharedString::from(kind_label(sym.kind))),
    );

    let mut container = div().flex().flex_col().child(pill);

    if !sym.children.is_empty() {
        // Children group: left border == the mermaid vertical connector.
        let mut group = div()
            .flex()
            .flex_col()
            .ml(px(9.))
            .pl(px(8.))
            .border_l_1()
            .border_color(rgb(theme.border));
        for child in &sym.children {
            group = group.child(node(child, theme, cx));
        }
        container = container.child(group);
    }

    container.into_any_element()
}

fn kind_label(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Namespace => "namespace",
        SymbolKind::Class => "class",
        SymbolKind::Struct => "struct",
        SymbolKind::Function => "fn",
        SymbolKind::Method => "method",
        SymbolKind::Member => "member",
        SymbolKind::Enum => "enum",
    }
}
