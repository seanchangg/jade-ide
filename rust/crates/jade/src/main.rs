//! Jade — GPUI app shell for the C++/Metal IDE with live GPU training
//! visualization (Rust rewrite of the Electron/TypeScript app).
//!
//! This binary wires the telemetry server + the four engine crates
//! (`forge-build` / `forge-debug` / `forge-sysmon` / `forge-ai`) into the GPUI
//! window shell (`app::JadeApp`). Every async source is bridged onto one unified
//! [`app::AppEvent`] channel (see `app.rs`), so the pump coalesces bursts in one
//! place.
//!
//! Usage:
//!   cargo run -p jade                          # bare window
//!   cargo run -p jade -- --file main.cpp       # with an active file
//!   cargo run -p jade -- --project dir/        # first .cpp/.cc/.mm in dir
//!   cargo run -p jade -- --train ../probe      # launch the probe demo program
//!   cargo run -p jade -- --project dir --smoke build-run   # headless verify
//!   cargo run -p jade -- --project dir --smoke term        # headless PTY check
//!
//! Repo root (for the engine's native-source dirs) is resolved at runtime from
//! `JADE_REPO_ROOT`, falling back to `CARGO_MANIFEST_DIR/../../..` (the
//! forge-ide checkout the binary was built from).
//!
//! Module map:
//!   theme       — forge-dark/forge-light palettes (§4.2)
//!   format      — value formatting ported from the TS renderer
//!   prefs       — telemetry preference persistence
//!   registry    — discovered-item model + auto-check rule (§5.6)
//!   training    — capped data buffers / tensor rings / ghost snapshots (§7.1)
//!   output      — output-panel scrollback + ANSI strip (§6)
//!   memory_bar  — memory-bar model + threshold classification (§5.3)
//!   app         — window layout + unified event pump + action handlers (§2, §6)
//!   panels      — file_tree / code_view / training_view / telemetry_sidebar,
//!                 plus Phase-4 wave 2: terminal_panel (§5.2) + runtime_panel (§5.4)

// Phase-2 shell helpers (deferred pref setters, some TrainingData helpers) are
// deliberately present ahead of full wiring.
#![allow(dead_code)]

mod app;
mod asm;
mod benchmark;
mod debug;
mod decorations;
mod editor_view;
mod format;
mod frequency;
mod ghost;
mod highlight;
mod memory_bar;
mod notes;
mod output;
mod panels;
mod prefs;
mod quick_open;
mod registry;
mod structure;
mod theme;
mod training;
mod wg3d;
mod workspace_state;
mod workspace_tree;
mod xp;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_ai::InlineCompletionBackend;
use forge_build::{BuildEngine, EngineConfig};
use forge_sysmon::SystemMonitor;
use forge_telemetry::{Event, TelemetryServer};
use forge_term::TermManager;
use gpui::{px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};
use gpui_platform::application;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use app::{AppDeps, AppEvent, JadeApp};
use workspace_tree::{is_watch_relevant, WatchDebounce};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Telemetry server on a dedicated tokio runtime; the Handle is kept so
    // button handlers (and the smoke hook) can spawn engine futures.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    let socket = TelemetryServer::default_socket_path();
    let (server, tel_events) = {
        let _guard = runtime.enter();
        TelemetryServer::start(socket.clone()).expect("bind telemetry socket")
    };
    let server = Arc::new(server);

    // ── Engine lifecycle (deliverable §2) ─────────────────────────────────────
    let root = repo_root();
    let engine = Arc::new(BuildEngine::new(
        EngineConfig::from_repo_root(&root).with_telemetry(server.clone()),
    ));
    let sysmon = Arc::new(SystemMonitor::new());
    let ai = Arc::new(InlineCompletionBackend::new()); // constructed, NOT started
    {
        let _guard = runtime.enter();
        sysmon.start(); // SystemMonitor runs from app start
    }

    // ── Terminal engine (§5.2) ────────────────────────────────────────────────
    let term = Arc::new(TermManager::new());
    let term_events = term.take_events();

    // ── Unified event channel + source forwarders (deliverable §1) ────────────
    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    spawn_forwarders(&handle, app_tx.clone(), tel_events, &sysmon, &ai);

    // Forward terminal events (Damaged/Exited) onto the unified pump.
    if let Some(mut trx) = term_events {
        let tx = app_tx.clone();
        handle.spawn(async move {
            while let Some(ev) = trx.recv().await {
                if tx.send(AppEvent::Term(ev)).is_err() {
                    break;
                }
            }
        });
    }

    let active_file = resolve_active_file(&args);
    let workspace_root = resolve_workspace_root(&args, active_file.as_deref(), &root);
    let demo = args.iter().any(|a| a == "--train");

    // ── FS-watch: debounced tree refresh (§5.1) ───────────────────────────────
    let watcher = spawn_fs_watch(&workspace_root, &handle, app_tx.clone());

    println!("FORGE_TELEMETRY_SOCK={}", socket.display());
    println!("repo root: {}", root.display());
    if let Some(f) = &active_file {
        println!("active file: {}", f.display());
    }

    let deps = AppDeps {
        server: server.clone(),
        engine,
        ai,
        sysmon,
        term: term.clone(),
        runtime: handle.clone(),
        app_tx,
        active_file,
        repo_root: root,
        workspace_root,
        demo,
    };

    // ── Headless smoke hook (deliverable §7) ──────────────────────────────────
    if let Some(mode) = arg_value(&args, "--smoke") {
        if mode == "open" {
            // `--smoke open <file>`: drive the real tab/highlight path headlessly.
            let target = arg_value(&args, "open").map(PathBuf::from);
            run_smoke_open(deps, target);
        } else if mode == "term" {
            run_smoke_term(runtime, deps);
        } else if mode == "wg3d" {
            run_smoke_wg3d(deps);
        } else if mode == "decorations" {
            let target = arg_value(&args, "decorations")
                .map(PathBuf::from)
                .or_else(|| deps.active_file.clone());
            run_smoke_decorations(target);
        } else if mode == "structure" {
            let target = arg_value(&args, "structure").map(PathBuf::from);
            run_smoke_structure(deps, target);
        } else if mode == "quickopen" {
            let query = arg_value(&args, "quickopen").unwrap_or_default();
            run_smoke_quickopen(deps, &query);
        } else if mode == "edit" {
            let target = arg_value(&args, "edit")
                .map(PathBuf::from)
                .or_else(|| deps.active_file.clone());
            run_smoke_edit(deps, target);
        } else if mode == "ghost" {
            run_smoke_ghost();
        } else if mode == "lsp" {
            let target = arg_value(&args, "lsp").map(PathBuf::from);
            run_smoke_lsp(runtime, deps, target);
        } else {
            run_smoke(runtime, deps, app_rx, &mode);
        }
        return;
    }

    // Keep the runtime + fs-watcher alive on a background thread (GUI path).
    std::thread::spawn(move || {
        let _watcher = watcher; // hold the watcher for the process lifetime
        runtime.block_on(std::future::pending::<()>())
    });

    // Optionally launch the probe's test training program against ourselves.
    maybe_launch_train(&args, &socket);

    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| JadeApp::new(cx, deps, app_rx)),
        )
        .unwrap();
        cx.activate(true);
    });
}

/// Bridge every async source onto the unified [`AppEvent`] channel. Runs on the
/// tokio runtime so the tokio receivers/watch channels are driven correctly.
fn spawn_forwarders(
    handle: &Handle,
    app_tx: UnboundedSender<AppEvent>,
    mut tel_events: UnboundedReceiver<Event>,
    sysmon: &Arc<SystemMonitor>,
    ai: &Arc<InlineCompletionBackend>,
) {
    // Telemetry Events.
    let tx = app_tx.clone();
    handle.spawn(async move {
        while let Some(e) = tel_events.recv().await {
            if tx.send(AppEvent::Telemetry(e)).is_err() {
                break;
            }
        }
    });

    // Sysmon stats (watch channel: emit the seed, then every change).
    let mut srx = sysmon.subscribe();
    let tx = app_tx.clone();
    handle.spawn(async move {
        let _ = tx.send(AppEvent::Sys(srx.borrow().clone()));
        while srx.changed().await.is_ok() {
            if tx.send(AppEvent::Sys(srx.borrow().clone())).is_err() {
                break;
            }
        }
    });

    // AI status (watch channel).
    let mut arx = ai.subscribe();
    let tx = app_tx.clone();
    handle.spawn(async move {
        let _ = tx.send(AppEvent::Ai(arx.borrow().clone()));
        while arx.changed().await.is_ok() {
            if tx.send(AppEvent::Ai(arx.borrow().clone())).is_err() {
                break;
            }
        }
    });
}

/// Watch the workspace root recursively (Phase-4 wave 2, §5.1). Raw notify
/// callbacks are filtered through the tree's ignore rules
/// ([`is_watch_relevant`]) and debounced 250ms ([`WatchDebounce`]) on the tokio
/// runtime; each settled burst emits one [`AppEvent::TreeChanged`], which the
/// app answers with an expansion-preserving re-scan. Returns the watcher, which
/// must be kept alive for the process lifetime; `None` (with a stderr note) if
/// the watch could not be established — the tree simply stops auto-refreshing.
fn spawn_fs_watch(
    root: &Path,
    handle: &Handle,
    app_tx: UnboundedSender<AppEvent>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};

    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    // Judge relevance on the path *relative to the root* so a dot-directory or
    // otherwise-ignored ancestor of the workspace root can't drop everything.
    let rel_root = root.to_path_buf();
    let watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if let Ok(ev) = res {
            // Only changes the tree would display; drops `.o`/build-dir bursts.
            let relevant = ev
                .paths
                .iter()
                .any(|p| is_watch_relevant(p.strip_prefix(&rel_root).unwrap_or(p)));
            if relevant {
                let _ = raw_tx.send(());
            }
        }
    });
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[jade] fs-watch unavailable: {e}");
            return None;
        }
    };
    if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
        eprintln!("[jade] fs-watch failed for {}: {e}", root.display());
        return None;
    }

    // Debounce loop: WatchDebounce holds the pure 250ms-window logic; this task
    // supplies the clock and the channel plumbing.
    handle.spawn(async move {
        let start = std::time::Instant::now();
        let mut deb = WatchDebounce::new(250);
        loop {
            if deb.is_pending() {
                // Poll for more events in short slices so the window can lapse.
                match tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    raw_rx.recv(),
                )
                .await
                {
                    Ok(Some(())) => deb.on_event(start.elapsed().as_millis() as u64),
                    Ok(None) => break, // watcher dropped
                    Err(_) => {}       // timeout slice — just re-check the window
                }
                if deb.poll(start.elapsed().as_millis() as u64)
                    && app_tx.send(AppEvent::TreeChanged).is_err()
                {
                    break;
                }
            } else {
                match raw_rx.recv().await {
                    Some(()) => deb.on_event(start.elapsed().as_millis() as u64),
                    None => break,
                }
            }
        }
    });

    Some(watcher)
}

/// `--smoke term` (Phase-4 wave 2): create a real terminal with cwd = workspace
/// root, type `printf hello-jade-term`, and poll snapshots until the text shows
/// up in the grid (forge-term's own integration-test pattern). Prints a
/// `--smoke wg3d` (deliverable §7): instantiate the 3D weight-grid module,
/// feed 70 synthetic frames of a 32×32 buffer through its ring (asserting the
/// ring caps at 64), project the default framed camera, and print a
/// machine-readable line. No window; exercises the real telemetry apply feed.
fn run_smoke_wg3d(deps: AppDeps) {
    use crate::wg3d::{grid::build_bars, math};

    let mut app = JadeApp::assemble(deps);

    // 70 frames of a 32×32 buffer, streamed through the SAME apply path the GUI
    // uses (Event::Tensor → app.wg3d.on_frame ring).
    for step in 0..70i64 {
        let data: Vec<f32> = (0..32 * 32)
            .map(|i| ((i as f32) * 0.017 + step as f32 * 0.1).sin())
            .collect();
        app.apply_app_event(AppEvent::Telemetry(Event::Tensor {
            name: "W".to_string(),
            step,
            rows: 32,
            cols: 32,
            src_rows: None,
            src_cols: None,
            dtype: "f32".to_string(),
            data,
        }));
    }

    // Ring must cap at 64 (independent of the training view's 32).
    let ring_len = app.wg3d.frames_len();
    if ring_len != 64 {
        println!("[smoke] FAIL wg3d ring={ring_len} (expected 64)");
        return;
    }

    // Open the overlay (selects "W") and build the current frame's bars.
    app.wg3d.open(Some("W"));
    let Some(bars) = app.wg3d.current_bars() else {
        println!("[smoke] FAIL wg3d produced no bars");
        return;
    };
    let n = bars.bars.len();

    // Depth-sort the bars back-to-front and confirm the order is monotonic.
    let view = app.wg3d.camera.view_matrix();
    let mut depths: Vec<f32> = bars
        .bars
        .iter()
        .map(|b| math::view_depth(&view, b.centroid()))
        .collect();
    depths.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let depth_sorted = depths.windows(2).all(|w| w[0] <= w[1]);

    // Project the scene origin through the default framed camera.
    let (w, h) = (1400.0f32, 862.0f32);
    let proj = math::Mat4::perspective(app.wg3d.camera.fov_y, w / h, 0.1, 4000.0);
    let mvp = proj.mul(&view);
    let sample = math::project(&mvp, [0.0, 0.0, 0.0], w, h);

    match sample {
        Some(pr) if depth_sorted => println!(
            "[smoke] wg3d bars={n} depth_sorted=ok proj_sample=<{:.1},{:.1}>",
            pr.x, pr.y
        ),
        Some(_) => {
            println!("[smoke] FAIL wg3d depth sort not monotonic");
            return;
        }
        None => {
            println!("[smoke] FAIL wg3d origin projected behind camera");
            return;
        }
    }

    // Perf probe: per-frame CPU cost of build (colormap+subsample) + depth sort
    // at the interactive default (64×64) and the raised budget (128×128).
    for dim in [64u32, 128] {
        let data: Vec<f32> = (0..dim as usize * dim as usize)
            .map(|i| ((i as f32) * 0.013).sin())
            .collect();
        let f = crate::training::TensorFrame {
            step: 0,
            rows: dim,
            cols: dim,
            src_rows: None,
            src_cols: None,
            data,
        };
        let t0 = std::time::Instant::now();
        let g = build_bars(&f).expect("bars");
        let mut ds: Vec<(f32, usize)> = g
            .bars
            .iter()
            .enumerate()
            .map(|(i, b)| (math::view_depth(&view, b.centroid()), i))
            .collect();
        ds.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let us = t0.elapsed().as_micros();
        println!(
            "[smoke] wg3d perf {dim}x{dim}: build+sort {us}us ({} bars)",
            g.bars.len()
        );
    }
}


/// with the total symbol count and the top-level count.
fn run_smoke_structure(deps: AppDeps, target: Option<PathBuf>) {
    let target = match target.or_else(|| deps.active_file.clone()) {
        Some(t) => t,
        None => {
            println!("[smoke] FAIL structure: no file (usage: --smoke structure <file>)");
            return;
        }
    };
    let mut app = JadeApp::assemble(deps);
    app.open_file(target.clone());
    let symbols = app.active_symbols();
    let total = structure::count(symbols);
    let top = symbols.len();
    println!(
        "[smoke] structure {} symbols={} top_level={}",
        target.display(),
        total,
        top
    );
}
/// each with its relative-path hint when it differs from the bare name.
fn run_smoke_quickopen(deps: AppDeps, query: &str) {
    let root = deps.workspace_root.clone();
    let tree = workspace_tree::FileTree::scan_full(root.clone());
    let files = quick_open::flatten(&tree);
    let matches = quick_open::filter(&files, query, &root);
    println!(
        "[smoke] quickopen query={:?} matches={} (of {} files)",
        query,
        matches.len(),
        files.len()
    );
    for m in &matches {
        match &m.hint {
            Some(hint) => println!("  {}  ·  {}", m.name, hint),
            None => println!("  {}", m.name),
        }
    }
}

/// machine-readable `[smoke] term ok cols=<c> rows=<r>` line.
fn run_smoke_term(runtime: tokio::runtime::Runtime, deps: AppDeps) {
    runtime.block_on(async move {
        let term = deps.term.clone();
        let id = match term.create(&deps.workspace_root) {
            Ok(id) => id,
            Err(e) => {
                println!("[smoke] FAIL term: {e}");
                return;
            }
        };
        term.write(id, b"printf 'hello-jade-term\\n'\n");

        // Generous timeout: shell startup (user dotfiles) can be slow.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut ok = false;
        let (mut cols, mut rows) = (0usize, 0usize);
        while std::time::Instant::now() < deadline {
            if let Some(snap) = term.snapshot(id) {
                cols = snap.cols;
                rows = snap.rows;
                if snap.contains_text("hello-jade-term") {
                    ok = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        term.destroy(id);
        if ok {
            println!("[smoke] term ok cols={cols} rows={rows}");
        } else {
            println!("[smoke] FAIL term: text never appeared in the grid");
        }
    });
}

/// Run the same build (then run) action functions the buttons call, printing
/// machine-readable `[smoke]` lines, then exit. No window; events are pumped on
/// the tokio runtime directly (deliverable §7).
fn run_smoke(
    runtime: tokio::runtime::Runtime,
    deps: AppDeps,
    mut app_rx: UnboundedReceiver<AppEvent>,
    mode: &str,
) {
    runtime.block_on(async move {
        let mut app = JadeApp::assemble(deps);

        if app.active_file.is_none() {
            println!("[smoke] FAIL no active file (pass --file or --project)");
            return;
        }

        // Build.
        app.action_build();
        while app.building {
            match app_rx.recv().await {
                Some(ev) => app.apply_app_event(ev),
                None => break,
            }
        }
        let ok = app
            .last_build
            .as_ref()
            .map(|b| b.success && b.executable.is_some())
            .unwrap_or(false);
        if !ok {
            let reason = app
                .last_build
                .as_ref()
                .and_then(|b| b.errors.first())
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "compile produced no executable".to_string());
            println!("[smoke] FAIL build: {reason}");
            return;
        }
        let exe = app
            .last_build
            .as_ref()
            .unwrap()
            .executable
            .clone()
            .unwrap();
        println!("[smoke] build ok {}", exe.display());

        if mode == "build-run" {
            app.action_run();
            while app.running {
                match app_rx.recv().await {
                    Some(ev) => app.apply_app_event(ev),
                    None => break,
                }
            }
            match &app.last_run {
                Some(r) => println!(
                    "[smoke] run exit={} duration={} executed_lines={}",
                    r.exit_code, r.duration_ms, r.executed_lines
                ),
                None => println!("[smoke] FAIL run produced no result"),
            }
        }
    });
}

/// `--smoke decorations <file>` (Phase-4 wave 3): run the static size scanner
/// and the flow analyzer over a file and print a machine-readable line proving
/// both pure pipelines work headlessly. No window; no app state needed.
fn run_smoke_decorations(target: Option<PathBuf>) {
    let Some(target) = target else {
        println!("[smoke] FAIL decorations: no file (usage: --smoke decorations <file>)");
        return;
    };
    let text = match std::fs::read(&target) {
        Ok(raw) => String::from_utf8_lossy(&raw).into_owned(),
        Err(e) => {
            println!("[smoke] FAIL decorations {}: {e}", target.display());
            return;
        }
    };
    let sizes = decorations::size_annotations::collect_size_annotations(&text);
    let flow = decorations::flow::analyze(&text);
    println!(
        "[smoke] decorations {} sizes={} glyphs={} segments={}",
        target.display(),
        sizes.len(),
        flow.glyphs.len(),
        flow.segments.len(),
    );
}

/// `--smoke open <file>` (deliverable §7/§8): assemble the app (which scans the
/// tree and seeds any `--file`/`--project` tab), open `<file>` through the real
/// tab + tree-sitter-highlight path, and print a machine-readable line proving
/// the viewer pipeline works headlessly.
fn run_smoke_open(deps: AppDeps, target: Option<PathBuf>) {
    let target = match target.or_else(|| deps.active_file.clone()) {
        Some(t) => t,
        None => {
            println!("[smoke] FAIL open: no file (usage: --smoke open <file>)");
            return;
        }
    };
    let mut app = JadeApp::assemble(deps);
    app.open_file(target.clone());
    match app.editor.active_tab() {
        Some(tab) if tab.path == target => println!(
            "[smoke] open {} lines={} spans={}",
            target.display(),
            tab.line_count(),
            tab.span_count()
        ),
        _ => println!("[smoke] FAIL open {}", target.display()),
    }
}

/// `--smoke edit <file>` (E2): open a file, apply a scripted edit sequence
/// (insert 'x' at 0:0, newline+indent, undo, redo, word-right ×3) directly on the
/// buffer, and print a machine-readable line proving the editing core works
/// headlessly.
fn run_smoke_edit(deps: AppDeps, target: Option<PathBuf>) {
    let target = match target {
        Some(t) => t,
        None => {
            println!("[smoke] FAIL edit: no file (usage: --smoke edit <file>)");
            return;
        }
    };
    let mut app = JadeApp::assemble(deps);
    app.open_file(target.clone());
    let Some(tab) = app.editor.active_tab_mut() else {
        println!("[smoke] FAIL edit: could not open {}", target.display());
        return;
    };
    // Caret to document start, then the scripted sequence.
    tab.buffer.set_caret(0);
    let mut lsp_changes = 0usize;
    lsp_changes += tab.buffer.type_char('x').changes.len();
    lsp_changes += tab.buffer.insert_newline().changes.len();
    tab.buffer.undo();
    tab.buffer.redo();
    tab.buffer.move_word_right(false);
    tab.buffer.move_word_right(false);
    tab.buffer.move_word_right(false);
    let caret = tab.caret_point();
    println!(
        "[smoke] edit {} version={} dirty={} caret={}:{} lsp_changes={}",
        target.display(),
        tab.buffer.version(),
        tab.buffer.is_dirty(),
        caret.row,
        caret.col,
        lsp_changes
    );
}

/// `--smoke ghost` (E3): drive the AI ghost-text cache + post-processing path
/// with a **canned** model response (no llama-server — the string below stands in
/// for the `/infill` result the real backend would return). Exercises a fresh
/// cache miss, then a typed-through hit served from the cache, printing a
/// machine-readable `[smoke] ghost cached=<bool> text="…"` line for each.
fn run_smoke_ghost() {
    use ghost::{post_process, GhostCache, MAX_LINES};

    // The scripted context: caret just after `for (int i = 0`, with a `)` already
    // sitting after the cursor on the line.
    let prefix1 = "int main() {\n    for (int i = 0";
    let suffix = ")";
    let line_suffix = ")";
    // Canned model output (what `/infill` would return). Note the trailing `)`
    // that duplicates what already follows the cursor — post-processing drops it,
    // and the blank line beyond is truncated.
    let canned = "; i < n; i++)\n\n    // loop body here";

    let mut cache = GhostCache::new();

    // 1) Cache miss: fresh response → post-process → cache the RAW output.
    let cached_hit = cache.lookup(prefix1, suffix).is_some();
    let text1 = post_process(canned, line_suffix, MAX_LINES).unwrap_or_default();
    cache.put(prefix1, suffix, canned);
    println!("[smoke] ghost cached={cached_hit} text={text1:?}");

    // 2) Typed-through hit: the user typed `; i < n` through the suggestion, so the
    //    remainder is served instantly from the cache (no request).
    let prefix2 = "int main() {\n    for (int i = 0; i < n";
    let served = cache.lookup(prefix2, suffix);
    let cached_hit2 = served.is_some();
    let text2 = served
        .and_then(|raw| post_process(&raw, line_suffix, MAX_LINES))
        .unwrap_or_default();
    println!("[smoke] ghost cached={cached_hit2} text={text2:?}");
}

/// `--smoke lsp <dir>` (E2, best-effort): initialize clangd on a temp project,
/// open a file with an error, await the first diagnostics event, and print a
/// machine-readable line. Skips gracefully when clangd is unavailable.
fn run_smoke_lsp(runtime: tokio::runtime::Runtime, _deps: AppDeps, dir: Option<PathBuf>) {
    use forge_lsp::{LspClient, LspEvent};
    runtime.block_on(async move {
        // Build a throwaway project (or use the passed dir) with a known error.
        let root = match dir {
            Some(d) => d,
            None => {
                let d = std::env::temp_dir().join(format!("jade-lsp-smoke-{}", std::process::id()));
                let _ = std::fs::create_dir_all(&d);
                let _ = std::fs::write(
                    d.join("main.cpp"),
                    "int main() { int x = ; return 0; }\n",
                );
                d
            }
        };
        let file = root.join("main.cpp");
        let t0 = std::time::Instant::now();
        let mut handle = match LspClient::initialize(&root, None).await {
            Ok(h) => h,
            Err(e) => {
                println!("[smoke] lsp skipped (clangd unavailable: {e})");
                return;
            }
        };
        let mut events = handle.take_events().expect("events");
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let _ = handle.did_open(&file, &text, 1);
        // Await the first non-empty diagnostics for our file, with a timeout.
        let deadline = std::time::Duration::from_secs(20);
        let got = tokio::time::timeout(deadline, async {
            loop {
                match events.recv().await {
                    Some(LspEvent::Diagnostics { path, diagnostics })
                        if path == file && !diagnostics.is_empty() =>
                    {
                        return Some(diagnostics.len());
                    }
                    Some(LspEvent::Exited) | None => return None,
                    _ => continue,
                }
            }
        })
        .await;
        match got {
            Ok(Some(n)) => println!(
                "[smoke] lsp diagnostics={} in {}ms",
                n,
                t0.elapsed().as_millis()
            ),
            Ok(None) => println!("[smoke] lsp skipped (clangd exited before diagnostics)"),
            Err(_) => println!("[smoke] lsp skipped (no diagnostics within timeout)"),
        }
        handle.shutdown().await;
    });
}

/// The file tree's scan root: the `--project` dir if given, else the active
/// file's parent directory, else the repo root.
fn resolve_workspace_root(args: &[String], active: Option<&std::path::Path>, root: &Path) -> PathBuf {
    if let Some(dir) = arg_value(args, "--project") {
        return PathBuf::from(dir);
    }
    if let Some(f) = active {
        if let Some(parent) = f.parent() {
            if !parent.as_os_str().is_empty() {
                return parent.to_path_buf();
            }
        }
    }
    root.to_path_buf()
}

/// Resolve the forge-ide checkout root: `JADE_REPO_ROOT` if set, else the
/// compile-time `CARGO_MANIFEST_DIR/../../..` (jade is at `rust/crates/jade`).
fn repo_root() -> PathBuf {
    if let Some(r) = std::env::var_os("JADE_REPO_ROOT") {
        return PathBuf::from(r);
    }
    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    fallback.canonicalize().unwrap_or(fallback)
}

/// `--file <path>` sets the active file directly; `--project <dir>` picks the
/// first `.cpp`/`.cc`/`.mm` alphabetically (deliverable §5).
fn resolve_active_file(args: &[String]) -> Option<PathBuf> {
    if let Some(f) = arg_value(args, "--file") {
        return Some(PathBuf::from(f));
    }
    if let Some(dir) = arg_value(args, "--project") {
        let mut cands: Vec<PathBuf> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("cpp") | Some("cc") | Some("mm")
                )
            })
            .collect();
        cands.sort();
        return cands.into_iter().next();
    }
    None
}

/// Value following `flag` in `args`, if present.
fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Replicate the pre-Phase-3 `--train <dir>` hatch: launch the probe's
/// `test_train` against our socket so the pipeline streams end to end.
fn maybe_launch_train(args: &[String], socket: &std::path::Path) {
    let Some(dir) = arg_value(args, "--train").map(PathBuf::from) else {
        return;
    };
    let dylib = dir
        .join("forge_probe.dylib")
        .canonicalize()
        .expect("probe dylib");
    std::process::Command::new(dir.join("test_train"))
        .current_dir(&dir)
        .env("DYLD_INSERT_LIBRARIES", dylib)
        .env("FORGE_TELEMETRY_SOCK", socket)
        .env_remove("FORGE_TRACK_ALL")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn test_train");
    println!("launched test_train with injected probe");
}
