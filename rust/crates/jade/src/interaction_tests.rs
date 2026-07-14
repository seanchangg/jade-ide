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
