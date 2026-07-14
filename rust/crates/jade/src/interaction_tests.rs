//! Headless interaction tests: real clicks + keystrokes through GPUI's full
//! window dispatch (layout, hitboxes, focus, key routing) via test-support —
//! the same harness Zed's editor tests use. These exist because the pure-logic
//! smokes (`--smoke edit`) can pass while the event plumbing in a live window
//! is broken; this is the layer that proves a click actually reaches
//! `editor_mouse_down` and an arrow key actually reaches `editor_key`.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{point, px, Modifiers, TestAppContext};

use crate::app::{AppDeps, JadeApp};
use forge_ai::InlineCompletionBackend;
use forge_build::{BuildEngine, EngineConfig};
use forge_sysmon::SystemMonitor;
use forge_telemetry::TelemetryServer;
use forge_term::TermManager;

/// A throwaway workspace with one known C++ file. Rows (0-based):
/// 0: `int main() {`
/// 1: `    int alpha = 1;`
/// 2: `    int beta = 2;`
/// 3: `    return alpha + beta;`
/// 4: `}`
fn test_workspace() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "jade-itest-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("t").replace("::", "-")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.cpp");
    std::fs::write(
        &file,
        "int main() {\n    int alpha = 1;\n    int beta = 2;\n    return alpha + beta;\n}\n",
    )
    .unwrap();
    (dir, file)
}

/// Assemble real AppDeps against the throwaway workspace. The tokio runtime is
/// leaked so engine handles stay valid for the life of the test process.
fn test_deps(workspace: PathBuf) -> (AppDeps, tokio::sync::mpsc::UnboundedReceiver<crate::app::AppEvent>) {
    // A current-thread runtime that is never driven: tasks (telemetry accept
    // loop, engine futures) queue but never poll, so no foreign thread runs
    // during the test — gpui's determinism checker requires that. The editor
    // interaction under test is all synchronous on the gpui thread.
    let runtime = Box::leak(Box::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap(),
    ));
    // Unix sockets cap at ~104 bytes of path; keep it short and unique.
    static SOCK_N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let socket = PathBuf::from(format!(
        "/tmp/jade-it-{}-{}.sock",
        std::process::id(),
        SOCK_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&socket);
    let _guard = runtime.enter(); // TelemetryServer::start needs a reactor
    let (server, _tel_rx) = TelemetryServer::start(socket).expect("bind telemetry socket");
    let server = Arc::new(server);
    let engine = Arc::new(BuildEngine::new(
        EngineConfig::from_repo_root(&workspace).with_telemetry(server.clone()),
    ));
    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel();
    let deps = AppDeps {
        server,
        engine,
        ai: Arc::new(InlineCompletionBackend::new()),
        sysmon: Arc::new(SystemMonitor::new()), // constructed, not started
        term: Arc::new(TermManager::new()),
        runtime: runtime.handle().clone(),
        app_tx,
        active_file: None,
        repo_root: workspace.clone(),
        workspace_root: workspace,
        // The test workspace is "open" (it has a real tree); the fs-watch is a
        // no-op so no notify thread runs under the determinism checker.
        workspace_opened: true,
        fs_watch: Arc::new(|_root| None),
        demo: false,
    };
    (deps, app_rx)
}

#[gpui::test]
async fn click_focuses_sets_caret_and_arrows_navigate(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Opening a file must focus the editor and paint a caret with NO click —
    // this was the original "I can't find my cursor / arrows are dead" bug:
    // nothing granted the editor focus until the user happened to click code.
    app.update_in(cx, |app, window, _cx| {
        let focused = app
            .editor_focus
            .as_ref()
            .map(|f| f.is_focused(window))
            .unwrap_or(false);
        assert!(focused, "opening a file must focus the editor");
    });
    assert!(
        cx.debug_bounds("editor-caret").is_some(),
        "caret bar must be painted right after open"
    );

    // Row 1 is "    int alpha = 1;". Click near column 6 (inside "int").
    let cell = cx
        .debug_bounds("code-cell-1")
        .expect("code cell for row 1 was painted");
    let x = cell.origin.x + px(6.0 * crate::panels::code_view::CHAR_W + 1.0);
    let y = cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0);
    cx.simulate_click(point(x, y), Modifiers::default());

    // The click must focus the editor and place the caret at row 1.
    app.update_in(cx, |app, window, _cx| {
        let focused = app
            .editor_focus
            .as_ref()
            .map(|f| f.is_focused(window))
            .unwrap_or(false);
        assert!(focused, "editor focus handle not focused after click");
        let caret = app.editor.active_tab().unwrap().caret_point();
        assert_eq!(caret.row, 1, "caret row after click");
        assert_eq!(caret.col, 6, "caret col after click");
    });

    // The caret bar must actually be painted now.
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-caret").is_some(),
        "caret bar not painted after click+focus"
    );

    // Arrows: down then left must move the caret through the buffer.
    cx.simulate_keystrokes("down");
    app.update_in(cx, |app, _w, _cx| {
        let caret = app.editor.active_tab().unwrap().caret_point();
        assert_eq!((caret.row, caret.col), (2, 6), "caret after down");
    });
    cx.simulate_keystrokes("left left");
    app.update_in(cx, |app, _w, _cx| {
        let caret = app.editor.active_tab().unwrap().caret_point();
        assert_eq!((caret.row, caret.col), (2, 4), "caret after left left");
    });
}

/// `open_project` state transition (§2), headless via `assemble` — no GPUI
/// context needed since `open_project` is `cx`-free. Opening folder B repoints
/// the tree, restores B's persisted active tab, and follows `active_file`.
#[test]
fn open_project_transitions_state() {
    let (dir_a, _file_a) = test_workspace();
    let (deps, _rx) = test_deps(dir_a.clone());
    let mut app = JadeApp::assemble(deps);

    // Folder B: two source files + a persisted ui blob restoring util.cpp as the
    // active tab plus a breakpoint, exactly like a real workspace.json.
    let dir_b = std::env::temp_dir().join(format!("jade-openproj-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_b);
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_b.join("alpha.cpp"), "int main(){return 0;}\n").unwrap();
    std::fs::write(dir_b.join("util.cpp"), "int u(){return 1;}\n").unwrap();
    let util = dir_b.join("util.cpp").display().to_string();
    let ui = crate::workspace_state::WorkspaceUi {
        open_tabs: vec![crate::workspace_state::TabState {
            path: util.clone(),
            is_dirty: false,
        }],
        active_tab_index: Some(0),
        breakpoints: std::collections::HashMap::from([(util.clone(), vec![2u32])]),
        ..Default::default()
    };
    crate::workspace_state::save(&dir_b, &ui);

    app.open_project(dir_b.clone());

    assert!(app.workspace_opened);
    assert_eq!(app.tree.as_ref().unwrap().root, dir_b, "tree repointed to B");
    assert_eq!(
        app.active_file.as_deref(),
        Some(dir_b.join("util.cpp").as_path()),
        "restored active tab drives active_file"
    );
    assert!(
        app.breakpoints.to_map().contains_key(&util),
        "B's persisted breakpoints restored"
    );

    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// `open_project` on a folder with no persisted tabs falls back to the first
/// source file alphabetically (mirrors `--project`).
#[test]
fn open_project_falls_back_to_first_source_file() {
    let (dir_a, _file_a) = test_workspace();
    let (deps, _rx) = test_deps(dir_a.clone());
    let mut app = JadeApp::assemble(deps);

    let dir_b = std::env::temp_dir().join(format!("jade-openproj2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_b);
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_b.join("zeta.cpp"), "int z(){return 0;}\n").unwrap();
    std::fs::write(dir_b.join("beta.cpp"), "int b(){return 0;}\n").unwrap();

    app.open_project(dir_b.clone());
    assert_eq!(
        app.active_file.as_deref(),
        Some(dir_b.join("beta.cpp").as_path()),
        "first source file alphabetically opens"
    );
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
}

/// Welcome mode (§2): assembled with `workspace_opened=false`, the tree is NOT
/// scanned (no fallback repo-root scan) and there is no active file.
#[test]
fn no_workspace_leaves_tree_unscanned() {
    let (dir, _file) = test_workspace();
    let (mut deps, _rx) = test_deps(dir.clone());
    deps.workspace_opened = false;
    deps.active_file = None;
    let app = JadeApp::assemble(deps);
    assert!(!app.workspace_opened);
    assert!(app.tree.is_none(), "welcome mode must not scan a tree");
    assert!(app.active_file.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[gpui::test]
async fn typing_inserts_through_input_pipeline(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Click at the very start of row 2 ("    int beta = 2;").
    let cell = cx
        .debug_bounds("code-cell-2")
        .expect("code cell for row 2 was painted");
    cx.simulate_click(
        point(
            cell.origin.x + px(1.0),
            cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
        ),
        Modifiers::default(),
    );

    // Type through the platform input pipeline (IME replace_text_in_range).
    cx.simulate_input("zz");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(
            tab.line(2),
            "zz    int beta = 2;",
            "typed text did not reach the buffer"
        );
        assert!(tab.buffer.is_dirty());
    });

    // Backspace through the key pipeline.
    cx.simulate_keystrokes("backspace");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.editor.active_tab().unwrap().line(2), "z    int beta = 2;");
    });
}

/// Regression: a plain click (down + the tiny pressed-button move macOS sends
/// before up) on a VISIBLE row must never scroll the list. This was the
/// "editor constantly jumps around when I click" bug.
#[gpui::test]
async fn click_on_visible_row_does_not_scroll(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("jade-noscroll-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.cpp");
    let body: String = (0..300)
        .map(|i| format!("static int line_{i} = {i};\n"))
        .collect();
    std::fs::write(&file, body).unwrap();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    let (top0, rows) = app.update_in(cx, |app, _w, _cx| {
        (
            app.editor_scroll_top(),
            app.editor_rows.load(std::sync::atomic::Ordering::Relaxed),
        )
    });
    println!("top0={top0} rows={rows}");

    // Click a row in the middle of the viewport, with the drag-move a real
    // click generates.
    let probe = top0 + (rows as usize) / 2;
    let cell = cx
        .debug_bounds(match probe {
            _ => Box::leak(format!("code-cell-{probe}").into_boxed_str()) as &'static str,
        })
        .expect("probe row visible");
    let pos = gpui::point(
        cell.origin.x + px(30.),
        cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
    );
    cx.simulate_mouse_down(pos, gpui::MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(pos, gpui::MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_up(pos, gpui::MouseButton::Left, Modifiers::default());
    cx.run_until_parked();

    let top1 = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
    assert_eq!(
        top1, top0,
        "clicking a visible row must not scroll (top {top0} -> {top1})"
    );
}

/// Same as above but with the list scrolled deep into the file — exercises
/// `editor_scroll_top()` (top_item) accuracy, including the PAD_TOP offset.
#[gpui::test]
async fn click_on_visible_row_when_scrolled_does_not_scroll(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("jade-noscroll2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.cpp");
    let body: String = (0..300)
        .map(|i| format!("static int line_{i} = {i};\n"))
        .collect();
    std::fs::write(&file, body).unwrap();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Scroll deep, then re-read where the viewport actually is.
    app.update_in(cx, |app, _w, cx| {
        app.code_scroll
            .scroll_to_item(150, gpui::ScrollStrategy::Top);
        cx.notify();
    });
    cx.run_until_parked();
    let (top0, rows) = app.update_in(cx, |app, _w, _cx| {
        (
            app.editor_scroll_top(),
            app.editor_rows.load(std::sync::atomic::Ordering::Relaxed),
        )
    });
    println!("scrolled: top0={top0} rows={rows}");

    for offset in [1usize, (rows as usize) / 2, rows as usize - 2] {
        let probe = top0 + offset;
        let sel: &'static str = Box::leak(format!("code-cell-{probe}").into_boxed_str());
        let Some(cell) = cx.debug_bounds(sel) else {
            println!("row {probe} not painted; skipping");
            continue;
        };
        let pos = gpui::point(
            cell.origin.x + px(30.),
            cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
        );
        cx.simulate_mouse_down(pos, gpui::MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(pos, gpui::MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(pos, gpui::MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        let top1 = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
        assert_eq!(
            top1, top0,
            "clicking visible row {probe} (offset {offset}) scrolled {top0} -> {top1}"
        );
    }
}

/// Clicking to the RIGHT of a line's text (inside the row, past the last char)
/// must put the caret at that line's end; typing with the caret scrolled out of
/// view must follow-scroll so the caret is visible again.
#[gpui::test]
async fn click_past_eol_and_type_follow_scroll(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!("jade-eol-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.cpp");
    let body: String = (0..300)
        .map(|i| format!("static int line_{i} = {i};\n"))
        .collect();
    std::fs::write(&file, body).unwrap();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Click far to the right of row 3's short text — caret must land at its EOL.
    let cell = cx.debug_bounds("code-cell-3").expect("row 3 painted");
    let pos = gpui::point(
        cell.origin.x + cell.size.width - px(20.),
        cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
    );
    cx.simulate_click(pos, Modifiers::default());
    let eol = "static int line_3 = 3;".chars().count();
    app.update_in(cx, |app, _w, _cx| {
        let caret = app.editor.active_tab().unwrap().caret_point();
        assert_eq!(
            (caret.row, caret.col),
            (3, eol),
            "click right of code must set caret at that line's end"
        );
    });

    // Scroll the caret out of view (deep), then type: viewport must follow.
    app.update_in(cx, |app, _w, cx| {
        app.code_scroll
            .scroll_to_item(200, gpui::ScrollStrategy::Top);
        cx.notify();
    });
    cx.run_until_parked();
    let top_before = app.update_in(cx, |app, _w, _cx| app.editor_scroll_top());
    assert!(top_before > 100, "precondition: scrolled deep ({top_before})");

    cx.simulate_input("z");
    cx.run_until_parked();
    let (top_after, rows, caret_row) = app.update_in(cx, |app, _w, _cx| {
        (
            app.editor_scroll_top(),
            app.editor_rows.load(std::sync::atomic::Ordering::Relaxed) as usize,
            app.editor.active_tab().unwrap().caret_point().row,
        )
    });
    assert_eq!(caret_row, 3);
    assert!(
        top_after <= caret_row && caret_row < top_after + rows,
        "typing out of view must scroll the caret back into view (top {top_after}, rows {rows})"
    );
}

/// ⌘⌫ deletes the whole current line; ⌘← is line-aware Home: first press goes
/// to the text edge (first non-whitespace), second to column 0.
#[gpui::test]
async fn cmd_backspace_and_smart_home(cx: &mut TestAppContext) {
    let (dir, file) = test_workspace();
    let (deps, app_rx) = test_deps(dir);

    let (app, cx) = cx.add_window_view(|_window, cx| JadeApp::new(cx, deps, app_rx));
    app.update_in(cx, |app, _window, cx| {
        app.open_file(file.clone());
        cx.notify();
    });
    cx.run_until_parked();

    // Caret into row 1 ("    int alpha = 1;") at col 8.
    let cell = cx.debug_bounds("code-cell-1").expect("row 1 painted");
    cx.simulate_click(
        gpui::point(
            cell.origin.x + px(8.0 * crate::panels::code_view::CHAR_W + 1.0),
            cell.origin.y + px(crate::panels::code_view::LINE_H / 2.0),
        ),
        Modifiers::default(),
    );

    // Smart home: ⌘← → col 4 (text edge), again → col 0, again → back to 4.
    cx.simulate_keystrokes("cmd-left");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.editor.active_tab().unwrap().caret_point().col, 4);
    });
    cx.simulate_keystrokes("cmd-left");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.editor.active_tab().unwrap().caret_point().col, 0);
    });
    cx.simulate_keystrokes("cmd-left");
    app.update_in(cx, |app, _w, _cx| {
        assert_eq!(app.editor.active_tab().unwrap().caret_point().col, 4);
    });

    // ⌘⌫ deletes the whole line: row 1 becomes the old row 2.
    cx.simulate_keystrokes("cmd-backspace");
    app.update_in(cx, |app, _w, _cx| {
        let tab = app.editor.active_tab().unwrap();
        assert_eq!(tab.line(1), "    int beta = 2;", "line 1 deleted wholesale");
        assert!(tab.buffer.is_dirty());
    });
}
