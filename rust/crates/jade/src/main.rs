//! Phase-0 spike: a GPUI window rendering live GPU tensor heatmaps streamed by
//! the real Metal probe through the forge-telemetry crate.
//!
//! Proves the two rewrite unknowns end to end:
//!   1. GPUI as a library (window, view state, per-frame canvas painting)
//!   2. tokio-based telemetry server feeding a GPUI view without jank
//!
//! Run:  cargo run -p jade -- --train ../probe
//! (or start it bare and launch any instrumented program with
//!  FORGE_TELEMETRY_SOCK pointed at the printed socket path.)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use forge_telemetry::{Event, Kind, TelemetryServer};
use gpui::{
    canvas, div, fill, point, prelude::*, px, rgb, size, App, Bounds, Context, Rgba, Window,
    WindowBounds, WindowOptions,
};
use gpui_platform::application;
use tokio::sync::mpsc::UnboundedReceiver;

/// Jade dark palette (docs/jade-feature-inventory.md §4.2).
const BG: u32 = 0x1E1F22;
const PANEL: u32 = 0x2B2D30;
const TEXT: u32 = 0xDFE1E5;
const MUTED: u32 = 0x6B7A72;
const ACCENT: u32 = 0x56B389;

struct Frame {
    step: i64,
    rows: u32,
    cols: u32,
    data: Vec<f32>,
}

struct HeatmapView {
    server: Arc<TelemetryServer>,
    frames: HashMap<String, Frame>,
    selected: Option<String>,
    scalars: u64,
    timings: u64,
    tensors: u64,
}

impl HeatmapView {
    fn new(
        cx: &mut Context<Self>,
        server: Arc<TelemetryServer>,
        mut events: UnboundedReceiver<Event>,
    ) -> Self {
        cx.spawn(async move |this, cx| {
            // tokio's mpsc is executor-agnostic, so awaiting it on GPUI's
            // executor is fine. Coalesce bursts: drain everything queued, then
            // notify once — the probe can emit thousands of events/sec and we
            // only need one repaint per batch (the same rule the Electron app
            // learned the hard way; see docs/perf-findings.md).
            while let Some(first) = events.recv().await {
                let mut batch = vec![first];
                while let Ok(more) = events.try_recv() {
                    batch.push(more);
                }
                if this
                    .update(cx, |view, cx| {
                        for event in batch {
                            view.apply(event);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break; // window closed
                }
            }
        })
        .detach();

        Self {
            server,
            frames: HashMap::new(),
            selected: None,
            scalars: 0,
            timings: 0,
            tensors: 0,
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Decl { kind, name, .. } => {
                // Spike behavior: auto-track every discovered buffer at the
                // default resolution (the real UI puts checkboxes here).
                if kind == Kind::Buffer {
                    self.server.set_track(kind, &name, true, Some(128), None);
                }
            }
            Event::Scalar(_) => self.scalars += 1,
            Event::Timing(_) => self.timings += 1,
            Event::Tensor {
                name,
                step,
                rows,
                cols,
                data,
                ..
            } => {
                self.tensors += 1;
                if self.tensors == 1 || self.tensors % 25 == 0 {
                    println!("[spike] tensor #{}: {name} {rows}x{cols} step {step}", self.tensors);
                }
                if self.selected.is_none() {
                    self.selected = Some(name.clone());
                }
                self.frames.insert(name, Frame { step, rows, cols, data });
            }
        }
    }
}

/// The diverging blue→white→red colormap centered at 0, normalized by the
/// frame's max-abs — identical convention to training-view.ts:792-804.
fn diverging(value: f32, max_abs: f32) -> Rgba {
    let t = if max_abs > 0.0 {
        (value / max_abs).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    if t >= 0.0 {
        Rgba { r: 1.0, g: 1.0 - t, b: 1.0 - t, a: 1.0 }
    } else {
        Rgba { r: 1.0 + t, g: 1.0 + t, b: 1.0, a: 1.0 }
    }
}

impl Render for HeatmapView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let header = match self.selected.as_ref().and_then(|n| {
            self.frames.get(n).map(|f| (n.clone(), f.step, f.rows, f.cols))
        }) {
            Some((name, step, rows, cols)) => format!("{name}  {rows}×{cols}  step {step}"),
            None => "waiting for tensor frames…".to_string(),
        };
        let stats = format!(
            "scalars {}   timings {}   tensors {}   socket {}",
            self.scalars,
            self.timings,
            self.tensors,
            self.server.socket_path().display()
        );

        // Snapshot the selected frame's cells for the paint closure.
        let cells: Option<(u32, u32, Vec<f32>, f32)> =
            self.selected.as_ref().and_then(|n| self.frames.get(n)).map(|f| {
                let max_abs = f.data.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                (f.rows, f.cols, f.data.clone(), max_abs)
            });

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            // JetBrains Mono once we bundle it like the Electron app did;
            // Menlo is always present on macOS.
            .font_family("Menlo")
            .text_sm()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .bg(rgb(PANEL))
                    .child(div().text_color(rgb(ACCENT)).child("JADE — TRAINING SPIKE"))
                    .child(div().child(header))
                    .child(div().text_color(rgb(MUTED)).text_xs().child(stats)),
            )
            .child(
                div().flex_1().p_3().child(
                    canvas(
                        move |_, _, _| {},
                        move |bounds, _, window, _| {
                        let Some((rows, cols, data, max_abs)) = cells else {
                            return;
                        };
                        if rows == 0 || cols == 0 {
                            return;
                        }
                        // Fit the grid into the available bounds, preserving
                        // aspect, nearest-neighbor blocky cells like the 2D
                        // previews in the Electron app.
                        let cell = (bounds.size.width / cols as f32)
                            .min(bounds.size.height / rows as f32);
                        for r in 0..rows {
                            for c in 0..cols {
                                let v = data[(r * cols + c) as usize];
                                let origin = bounds.origin
                                    + point(cell * c as f32, cell * r as f32);
                                window.paint_quad(fill(
                                    Bounds { origin, size: size(cell, cell) },
                                    diverging(v, max_abs),
                                ));
                            }
                        }
                        },
                    )
                    // canvas defaults to a zero-sized leaf (Style::default());
                    // it must be told to fill its container.
                    .size_full(),
                ),
            )
    }
}

fn main() {
    // Telemetry server lives on a dedicated tokio runtime thread; events cross
    // into GPUI over the (executor-agnostic) tokio mpsc channel.
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
    if let Some(dir) = std::env::args()
        .skip_while(|a| a != "--train")
        .nth(1)
        .map(PathBuf::from)
    {
        let dylib = dir.join("forge_probe.dylib").canonicalize().expect("probe dylib");
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
        let bounds = Bounds::centered(None, size(px(900.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| HeatmapView::new(cx, server, events)),
        )
        .unwrap();
        cx.activate(true);
    });
}
