//! Jade — GPUI app shell for the C++/Metal IDE with live GPU training
//! visualization (Rust rewrite of the Electron/TypeScript app).
//!
//! This binary wires the telemetry server (on a dedicated tokio runtime) into
//! the GPUI window shell (`app::JadeApp`), and optionally launches the probe's
//! test training program so the whole pipeline streams end to end:
//!
//!   cargo run -p jade -- --train ../probe
//!
//! Or start it bare and point any instrumented program at the printed socket
//! via `FORGE_TELEMETRY_SOCK`.
//!
//! Module map:
//!   theme     — forge-dark/forge-light palettes (§4.2)
//!   format    — value formatting ported from the TS renderer
//!   prefs     — telemetry preference persistence (~/.config/jade/telemetry.json)
//!   registry  — discovered-item model + auto-check rule (§5.6)
//!   training  — capped data buffers / tensor rings / ghost snapshots (§7.1)
//!   app       — window layout + event pump (§2)
//!   panels    — training_view + telemetry_sidebar renderers

// Phase-2 shell: the full §4.2 palette, the deferred shape-editor pref setters,
// and the TrainingData run-lifecycle helpers (push_memory/clear/latest_tensor)
// are deliberately present ahead of the Phase-3 build/run/debug wiring.
#![allow(dead_code)]

mod app;
mod format;
mod panels;
mod prefs;
mod registry;
mod theme;
mod training;

use std::path::PathBuf;
use std::sync::Arc;

use forge_telemetry::TelemetryServer;
use gpui::{px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};
use gpui_platform::application;

use app::JadeApp;

fn main() {
    // Telemetry server on a dedicated tokio runtime thread; events cross into
    // GPUI over the executor-agnostic tokio mpsc channel.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let socket = TelemetryServer::default_socket_path();
    let (server, events) = {
        let _guard = runtime.enter();
        TelemetryServer::start(socket.clone()).expect("bind telemetry socket")
    };
    let server = Arc::new(server);
    std::thread::spawn(move || runtime.block_on(std::future::pending::<()>()));
    println!("FORGE_TELEMETRY_SOCK={}", socket.display());

    // Optionally launch the probe's test training program against ourselves.
    let demo = std::env::args().any(|a| a == "--train");
    if let Some(dir) = std::env::args()
        .skip_while(|a| a != "--train")
        .nth(1)
        .map(PathBuf::from)
    {
        let dylib = dir
            .join("forge_probe.dylib")
            .canonicalize()
            .expect("probe dylib");
        std::process::Command::new(dir.join("test_train"))
            .current_dir(&dir)
            .env("DYLD_INSERT_LIBRARIES", dylib)
            .env("FORGE_TELEMETRY_SOCK", &socket)
            .env_remove("FORGE_TRACK_ALL")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn test_train");
        println!("launched test_train with injected probe");
    }

    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| JadeApp::new(cx, server, events, demo)),
        )
        .unwrap();
        cx.activate(true);
    });
}
