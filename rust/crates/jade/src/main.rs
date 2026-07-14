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
//!   panels      — training_view + telemetry_sidebar renderers

// Phase-2 shell helpers (deferred pref setters, some TrainingData helpers) are
// deliberately present ahead of full wiring.
#![allow(dead_code)]

mod app;
mod editor_view;
mod format;
mod highlight;
mod memory_bar;
mod output;
mod panels;
mod prefs;
mod registry;
mod theme;
mod training;
mod workspace_tree;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_ai::InlineCompletionBackend;
use forge_build::{BuildEngine, EngineConfig};
use forge_sysmon::SystemMonitor;
use forge_telemetry::{Event, TelemetryServer};
use gpui::{px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};
use gpui_platform::application;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use app::{AppDeps, AppEvent, JadeApp};

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

    // ── Unified event channel + source forwarders (deliverable §1) ────────────
    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    spawn_forwarders(&handle, app_tx.clone(), tel_events, &sysmon, &ai);

    let active_file = resolve_active_file(&args);
    let workspace_root = resolve_workspace_root(&args, active_file.as_deref(), &root);
    let demo = args.iter().any(|a| a == "--train");

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
        } else {
            run_smoke(runtime, deps, app_rx, &mode);
        }
        return;
    }

    // Keep the runtime alive on a background thread (GUI path).
    std::thread::spawn(move || runtime.block_on(std::future::pending::<()>()));

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
            tab.lines.len(),
            tab.span_count()
        ),
        _ => println!("[smoke] FAIL open {}", target.display()),
    }
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
