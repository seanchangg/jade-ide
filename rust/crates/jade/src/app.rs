//! The Jade window shell (feature inventory §2): action-bar strip, floating-card
//! main area (left panel / center content / right runtime sidebar), a bottom
//! output panel + memory-bar strip, and a status strip. The runtime sidebar
//! hosts the TRAINING view + telemetry sidebar. `JadeApp` owns all telemetry +
//! engine state and the **unified event pump**; the panel modules render pure
//! projections of it.
//!
//! Phase-3 wiring (this file): every async source — telemetry `Event`s, build
//! output, `RunEvent`s (incl. `AllocBatch`), run/debug completion, `DebugEvent`s,
//! and sysmon/AI status — flows through a single [`AppEvent`] mpsc into
//! [`JadeApp::apply_app_event`]. Button handlers and the headless smoke hook call
//! the SAME `action_*` methods, which spawn engine futures on the stored tokio
//! [`Handle`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use forge_ai::{AiState, AiStatus, InlineCompletionBackend};
use forge_build::{
    BuildEngine, BuildResult, CompileRequest, RunConfig, RunEvent, RunResult, PROBE_DYLIB,
};
use forge_debug::{DebugEvent, LldbDriver};
use forge_sysmon::{SystemMonitor, SystemStats};
use forge_telemetry::{Event, Kind, TelemetryServer};
use gpui::{div, prelude::*, px, rgb, Context, Window};
use serde_json::{Map, Value};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

use crate::memory_bar::{project, Level, MemoryBarState};
use crate::output::push_output;
use crate::panels::{telemetry_sidebar, training_view};
use crate::prefs::TelemetryPrefs;
use crate::registry::{key_of, TelemetryRegistry, DEFAULT_MAX_DIM};
use crate::theme::Theme;
use crate::training::{TensorFrame, TrainingData};

/// The single unified event type crossing into the app pump. Every async source
/// (deliverable §1) is one of these variants, so burst-coalescing and the
/// one-notify-per-batch rule live in exactly one place.
pub enum AppEvent {
    /// Telemetry `Event` (scalars/timings/tensors/decls) — the pre-Phase-3 stream.
    Telemetry(Event),
    /// A line of cmake/compiler output during a build.
    BuildOutput(String),
    /// A build finished (success or failure).
    BuildDone(BuildResult),
    /// A run event: program output or a memory event (incl. `AllocBatch`).
    Run(RunEvent),
    /// A run finished with its terminal result.
    RunDone(RunResult),
    /// An LLDB debug event (output / stopped / exited).
    Debug(DebugEvent),
    /// A system-monitor stats snapshot (CPU/GPU/OS memory).
    Sys(SystemStats),
    /// An AI backend status change.
    Ai(AiStatus),
}

/// Everything [`JadeApp`] needs from `main` to be constructed — kept as one
/// struct so the GUI constructor and the headless smoke path share wiring.
pub struct AppDeps {
    pub server: Arc<TelemetryServer>,
    pub engine: Arc<BuildEngine>,
    pub ai: Arc<InlineCompletionBackend>,
    pub sysmon: Arc<SystemMonitor>,
    /// Handle to the tokio runtime; button handlers spawn engine futures here.
    pub runtime: Handle,
    /// The write end of the unified event channel — cloned into every future.
    pub app_tx: UnboundedSender<AppEvent>,
    pub active_file: Option<PathBuf>,
    pub repo_root: PathBuf,
    pub demo: bool,
}

/// Recorded stats from the last completed run, shown in the status area.
#[derive(Clone)]
pub struct RunStatus {
    pub exit_code: i32,
    pub duration_ms: u128,
    pub executed_lines: usize,
}

pub struct JadeApp {
    pub server: Arc<TelemetryServer>,
    pub registry: TelemetryRegistry,
    pub training: TrainingData,
    pub prefs: TelemetryPrefs,
    pub theme: Theme,

    // ── Phase-3 engine handles ────────────────────────────────────────────────
    engine: Arc<BuildEngine>,
    ai: Arc<InlineCompletionBackend>,
    sysmon: Arc<SystemMonitor>,
    runtime: Handle,
    app_tx: UnboundedSender<AppEvent>,
    repo_root: PathBuf,
    /// LLDB driver, constructed lazily on the first Debug (deliverable §2).
    driver: Option<Arc<AsyncMutex<LldbDriver>>>,

    // ── Build/run/debug lifecycle state ───────────────────────────────────────
    pub active_file: Option<PathBuf>,
    pub building: bool,
    pub running: bool,
    pub debugging: bool,
    pub last_build: Option<BuildResult>,
    last_sanitize: bool,
    last_instrument: bool,
    pub last_run: Option<RunStatus>,

    // ── Output panel + memory bar ─────────────────────────────────────────────
    pub output: Vec<String>,
    pub output_visible: bool,
    pub mem: MemoryBarState,
    pub sys_stats: SystemStats,
    pub ai_status: AiStatus,

    // Demo/telemetry counters (also drive the stdout log the spike printed).
    pub scalars_seen: u64,
    pub timings_seen: u64,
    pub tensors_seen: u64,
    demo: bool,
    /// Headless smoke-test hatch (`JADE_DEMO_ENABLE_BUFFERS=1`): auto-enables
    /// discovered buffers so `--train` streams tensor frames without a click.
    demo_enable_buffers: bool,
}

impl JadeApp {
    /// GUI constructor: assemble state and spawn the unified event pump on the
    /// GPUI executor. `app_rx` is the read end paired with `deps.app_tx`.
    pub fn new(
        cx: &mut Context<Self>,
        deps: AppDeps,
        mut app_rx: UnboundedReceiver<AppEvent>,
    ) -> Self {
        // Event pump: coalesce bursts — drain everything queued, apply, then one
        // notify per batch (the spike's rule; the probe emits thousands/sec).
        cx.spawn(async move |this, cx| {
            while let Some(first) = app_rx.recv().await {
                let mut batch = vec![first];
                while let Ok(more) = app_rx.try_recv() {
                    batch.push(more);
                }
                if this
                    .update(cx, |app, cx| {
                        for event in batch {
                            app.apply_app_event(event);
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

        Self::assemble(deps)
    }

    /// Headless constructor (smoke hook / tests): assemble state, no pump. The
    /// caller drives `app_rx` itself and calls [`apply_app_event`](Self::apply_app_event).
    pub fn assemble(deps: AppDeps) -> Self {
        Self {
            server: deps.server,
            registry: TelemetryRegistry::new(),
            training: TrainingData::new(),
            prefs: TelemetryPrefs::load(),
            theme: Theme::forge_dark(),

            engine: deps.engine,
            ai: deps.ai,
            sysmon: deps.sysmon,
            runtime: deps.runtime,
            app_tx: deps.app_tx,
            repo_root: deps.repo_root,
            driver: None,

            active_file: deps.active_file,
            building: false,
            running: false,
            debugging: false,
            last_build: None,
            last_sanitize: false,
            last_instrument: false,
            last_run: None,

            output: Vec::new(),
            output_visible: true,
            mem: MemoryBarState::default(),
            sys_stats: SystemStats::default(),
            ai_status: AiStatus {
                state: AiState::Disabled,
                detail: "Not started".to_string(),
                endpoint: None,
            },

            scalars_seen: 0,
            timings_seen: 0,
            tensors_seen: 0,
            demo: deps.demo,
            demo_enable_buffers: std::env::var_os("JADE_DEMO_ENABLE_BUFFERS").is_some(),
        }
    }

    // ── Unified event application ─────────────────────────────────────────────

    /// Apply one unified [`AppEvent`] to app state. Single choke point for every
    /// async source (deliverable §1).
    pub fn apply_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Telemetry(e) => {
                let before = self.scalars_seen;
                self.apply(e);
                if self.demo && before / 200 != self.scalars_seen / 200 {
                    println!(
                        "[jade] scalars {} timings {} tensors {} (buffers stream only when a checkbox enables them)",
                        self.scalars_seen, self.timings_seen, self.tensors_seen
                    );
                }
            }
            AppEvent::BuildOutput(line) => push_output(&mut self.output, &line),
            AppEvent::BuildDone(res) => self.on_build_done(res),
            AppEvent::Run(ev) => self.on_run_event(ev),
            AppEvent::RunDone(res) => self.on_run_done(res),
            AppEvent::Debug(ev) => self.on_debug_event(ev),
            AppEvent::Sys(stats) => self.sys_stats = stats,
            AppEvent::Ai(status) => self.ai_status = status,
        }
    }

    fn status_line(&mut self, s: &str) {
        push_output(&mut self.output, s);
    }

    fn on_build_done(&mut self, res: BuildResult) {
        self.building = false;
        let ms = res.duration.as_millis();
        if res.success {
            self.status_line(&format!("[forge] Build succeeded ({ms}ms)"));
        } else {
            self.status_line(&format!(
                "[forge] Build failed ({} error(s), {ms}ms)",
                res.errors.len()
            ));
            // No editor exists yet, so jump-to-error is N/A — list them instead.
            for e in &res.errors {
                push_output(
                    &mut self.output,
                    &format!(
                        "  {}:{}:{}: {}",
                        e.file.display(),
                        e.line,
                        e.column,
                        e.message
                    ),
                );
            }
        }
        self.last_build = Some(res);
    }

    fn on_run_event(&mut self, ev: RunEvent) {
        match ev {
            RunEvent::Output(s) => push_output(&mut self.output, &s),
            RunEvent::Memory(m) => {
                if let Some(sample) = self.mem.apply(&m) {
                    // Keep the Memory chart moving (deliverable §6).
                    self.training.push_memory(sample);
                }
            }
        }
    }

    fn on_run_done(&mut self, res: RunResult) {
        self.running = false;
        let ms = res.duration.as_millis();
        let executed = res.executed_lines.len();
        if res.exit_code == 0 {
            self.status_line(&format!("[forge] Exited with code 0 ({ms}ms)"));
        } else {
            self.status_line(&format!(
                "[forge] Exited with code {} ({ms}ms)",
                res.exit_code
            ));
        }
        if res.interpose_active {
            self.status_line("[forge] Memory tracked via malloc interposer");
        }
        // Push the sanitizer summary lines to the output panel (deliverable §3).
        if let Some(san) = &res.sanitizer_output {
            for line in san.lines().take(60) {
                push_output(&mut self.output, line);
            }
        }
        self.last_run = Some(RunStatus {
            exit_code: res.exit_code,
            duration_ms: ms,
            executed_lines: executed,
        });
    }

    fn on_debug_event(&mut self, ev: DebugEvent) {
        match ev {
            DebugEvent::Output(s) => push_output(&mut self.output, &s),
            DebugEvent::Stopped {
                reason, file, line, ..
            } => {
                // Full debug UI is Phase-4; a status line is enough now.
                self.status_line(&format!("[forge] paused at {file}:{line} ({reason})"));
            }
            DebugEvent::Exited(code) => {
                self.debugging = false;
                self.status_line(&format!("[forge] debug exited ({code})"));
            }
        }
    }

    // ── Action-bar handlers (buttons AND the smoke hook call these) ───────────

    /// Build the active file (deliverable §3). Sanitizers off (malloc interposer
    /// is used instead, app.ts:1030); instrumentation off (no flow view yet).
    pub fn action_build(&mut self) {
        self.start_build(Vec::new(), false, false);
    }

    fn start_build(&mut self, flags: Vec<String>, sanitize: bool, instrument: bool) -> bool {
        let Some(file) = self.active_file.clone() else {
            self.status_line("[forge] No active file — pass --file or --project");
            return false;
        };
        if self.building {
            return false;
        }
        self.building = true;
        self.mem.reset(); // resetMemoryTracking at build time (app.ts:1024)
        self.output_visible = true;
        self.last_sanitize = sanitize;
        self.last_instrument = instrument;
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.status_line(&format!("[forge] Building {name}..."));

        let req = CompileRequest {
            file,
            flags,
            sanitize,
            instrument,
        };
        let engine = self.engine.clone();
        let tx = self.app_tx.clone();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let ftx = tx.clone();
        self.runtime.spawn(async move {
            while let Some(line) = out_rx.recv().await {
                let _ = ftx.send(AppEvent::BuildOutput(line));
            }
        });
        self.runtime.spawn(async move {
            let res = engine.compile(&req, &out_tx).await;
            drop(out_tx);
            let _ = tx.send(AppEvent::BuildDone(res));
        });
        true
    }

    /// Run the last successful build (deliverable §3). Enabled only after a
    /// successful build with an executable.
    pub fn action_run(&mut self) {
        let Some(build) = self.last_build.as_ref() else {
            self.status_line("[forge] Build first");
            return;
        };
        if !build.success {
            self.status_line("[forge] Last build failed — nothing to run");
            return;
        }
        let Some(exe) = build.executable.clone() else {
            self.status_line("[forge] Build produced no executable");
            return;
        };
        if self.running {
            return;
        }
        self.running = true;
        self.training.clear(); // ghost snapshot of the previous run
        self.mem.reset(); // reset run-memory state
        self.output_visible = true;
        let name = exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.status_line(&format!("[forge] Running ./{name}..."));

        let cfg = RunConfig {
            executable: exe,
            args: Vec::new(),
            enable_sanitizers: self.last_sanitize,
            enable_instrumentation: self.last_instrument,
        };
        let engine = self.engine.clone();
        let tx = self.app_tx.clone();
        self.runtime.spawn(async move {
            let handle = engine.run(cfg);
            let mut events = handle.events;
            while let Some(ev) = events.recv().await {
                let _ = tx.send(AppEvent::Run(ev));
            }
            let res = handle.result.await.unwrap_or_else(|_| RunResult {
                exit_code: -1,
                duration: Duration::ZERO,
                executed_lines: HashMap::new(),
                sanitizer_output: None,
                interpose_active: false,
                instrumentation_summary: None,
            });
            let _ = tx.send(AppEvent::RunDone(res));
        });
    }

    /// Debug the active file (deliverable §3): build with forced `-O0`, then
    /// start LLDB with the telemetry-socket + probe-dylib env seam.
    pub fn action_debug(&mut self) {
        let Some(file) = self.active_file.clone() else {
            self.status_line("[forge] No active file — pass --file or --project");
            return;
        };
        if self.building || self.debugging {
            return;
        }
        // Construct the driver + its event forwarder lazily on first Debug.
        if self.driver.is_none() {
            let (drv, mut drx) = LldbDriver::new();
            self.driver = Some(Arc::new(AsyncMutex::new(drv)));
            let tx = self.app_tx.clone();
            self.runtime.spawn(async move {
                while let Some(ev) = drx.recv().await {
                    if tx.send(AppEvent::Debug(ev)).is_err() {
                        break;
                    }
                }
            });
        }
        self.building = true;
        self.debugging = true;
        self.mem.reset();
        self.output_visible = true;
        self.status_line("[forge] Debug build (forced -O0)...");

        let req = CompileRequest {
            file,
            flags: vec!["-O0".to_string()],
            sanitize: false,
            instrument: false,
        };
        let engine = self.engine.clone();
        let driver = self.driver.clone().expect("driver constructed above");
        let tx = self.app_tx.clone();
        let sock = self.server.socket_path().display().to_string();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let ftx = tx.clone();
        self.runtime.spawn(async move {
            while let Some(line) = out_rx.recv().await {
                let _ = ftx.send(AppEvent::BuildOutput(line));
            }
        });
        self.runtime.spawn(async move {
            let res = engine.compile(&req, &out_tx).await;
            drop(out_tx);
            let success = res.success;
            let exe = res.executable.clone();
            let _ = tx.send(AppEvent::BuildDone(res));
            if let (true, Some(exe)) = (success, exe) {
                // env seam: telemetry socket always; probe dylib only if it built.
                let mut env = vec![("FORGE_TELEMETRY_SOCK".to_string(), sock)];
                if engine.ensure_probe_dylib() {
                    env.push((
                        "DYLD_INSERT_LIBRARIES".to_string(),
                        PROBE_DYLIB.to_string(),
                    ));
                }
                let cwd = exe
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| ".".to_string());
                let mut d = driver.lock().await;
                if let Err(e) = d.start(&exe.display().to_string(), &cwd, &[], &env).await {
                    let _ = tx.send(AppEvent::Debug(DebugEvent::Output(format!(
                        "lldb start failed: {e}\n"
                    ))));
                }
            }
        });
    }

    /// Stop everything (deliverable §3): kill the run, stop the debug session,
    /// reset lifecycle state.
    pub fn action_stop(&mut self) {
        self.engine.stop();
        if let Some(driver) = self.driver.clone() {
            self.runtime.spawn(async move {
                driver.lock().await.stop().await;
            });
        }
        self.building = false;
        self.running = false;
        self.debugging = false;
        self.status_line("[forge] Stopped");
    }

    /// Toggle the AI backend (deliverable §3): disabling kills the model server
    /// to free GPU/unified memory (app.ts:290). Status flows back via the watch
    /// channel forwarder.
    pub fn action_ai(&mut self) {
        let ai = self.ai.clone();
        let on = matches!(self.ai_status.state, AiState::Ready | AiState::Starting);
        self.runtime.spawn(async move {
            if on {
                ai.stop().await;
            } else {
                ai.start().await;
            }
        });
    }

    /// Swap forge-dark / forge-light (deliverable §3).
    pub fn action_theme(&mut self) {
        self.theme = if self.theme.is_light {
            Theme::forge_dark()
        } else {
            Theme::forge_light()
        };
    }

    /// Toggle the output panel visibility.
    pub fn action_toggle_output(&mut self) {
        self.output_visible = !self.output_visible;
    }

    /// True once a successful build with an executable exists (Run gating).
    pub fn can_run(&self) -> bool {
        self.last_build
            .as_ref()
            .map(|b| b.success && b.executable.is_some())
            .unwrap_or(false)
    }

    /// Apply one telemetry event to the registry + training buffers. Auto-check
    /// side effects (sending `track` to the probe) happen here; buffers are
    /// never auto-enabled (checkboxes rule now).
    fn apply(&mut self, event: Event) {
        match event {
            Event::Decl {
                kind,
                name,
                meta,
                renamed_from,
            } => {
                let (mr, mc) = meta_dims(meta.as_ref());
                if let Some(from) = renamed_from {
                    if from != name {
                        self.registry.rename(kind, &from, &name, &self.prefs);
                        self.prefs
                            .migrate(&key_of(kind, &from), &key_of(kind, &name));
                    }
                }
                let out = self.registry.declare(kind, &name, mr, mc, &self.prefs);
                if out.auto_enabled {
                    self.server.set_track(kind, &name, true, None, None);
                }
                if out.pref_enabled {
                    self.push_track(kind, &name, true);
                }
                if self.demo_enable_buffers
                    && kind == Kind::Buffer
                    && !self.registry.is_enabled(kind, &name)
                {
                    self.toggle_enabled(kind, &name); // smoke-test hatch only
                }
            }
            Event::Scalar(s) => {
                let out = self.registry.note_scalar(&s.name, s.step, s.value, &self.prefs);
                if out.auto_enabled {
                    self.server.set_track(Kind::Scalar, &s.name, true, None, None);
                }
                if out.pref_enabled {
                    self.push_track(Kind::Scalar, &s.name, true);
                }
                self.training.push_scalar(&s.name, s.step, s.value);
                self.scalars_seen += 1;
            }
            Event::Timing(t) => {
                self.registry.note_timing(&t.name, t.ms, t.step, &self.prefs);
                self.training.push_timing(&t.name, t.ms, t.step);
                self.timings_seen += 1;
            }
            Event::Tensor {
                name,
                step,
                rows,
                cols,
                src_rows,
                src_cols,
                data,
                ..
            } => {
                let out = self.registry.note_tensor(
                    &name,
                    src_rows.unwrap_or(rows),
                    src_cols.unwrap_or(cols),
                    step,
                    &self.prefs,
                );
                if out.pref_enabled {
                    self.push_track(Kind::Buffer, &name, true);
                }
                self.training.push_tensor(
                    &name,
                    TensorFrame {
                        step,
                        rows,
                        cols,
                        src_rows,
                        src_cols,
                        data,
                    },
                );
                self.tensors_seen += 1;
                if self.demo && (self.tensors_seen == 1 || self.tensors_seen % 25 == 0) {
                    println!(
                        "[jade] tensor #{}: {} {}x{} step {}",
                        self.tensors_seen, name, rows, cols, step
                    );
                }
            }
        }
    }

    /// User checkbox toggle: flip registry state, persist, send `track`.
    pub fn toggle_enabled(&mut self, kind: Kind, name: &str) {
        let now = !self.registry.is_enabled(kind, name);
        if !self.registry.set_enabled(kind, name, now) {
            return;
        }
        let key = key_of(kind, name);
        self.prefs.set_enabled(&key, now);
        self.prefs.save();
        self.push_track(kind, name, now);
    }

    /// Send `track` to the server with the item's persisted maxDim/shape.
    fn push_track(&self, kind: Kind, name: &str, enabled: bool) {
        let (max_dim, shape) = if kind == Kind::Buffer {
            let item = self.registry.get(kind, name);
            (
                item.and_then(|i| i.max_dim).or(Some(DEFAULT_MAX_DIM)),
                item.and_then(|i| i.effective_shape()),
            )
        } else {
            (None, None)
        };
        self.server.set_track(kind, name, enabled, max_dim, shape);
    }
}

fn meta_dims(meta: Option<&Map<String, Value>>) -> (Option<u32>, Option<u32>) {
    let get = |m: &Map<String, Value>, k: &str| m.get(k).and_then(|v| v.as_u64()).map(|n| n as u32);
    match meta {
        Some(m) => (get(m, "rows"), get(m, "cols")),
        None => (None, None),
    }
}

// ── Layout ───────────────────────────────────────────────────────────────────

impl Render for JadeApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(theme.bg))
            .text_color(rgb(theme.text))
            .font_family("Menlo") // JetBrains Mono isn't installed on this machine
            .text_sm()
            .child(action_bar(self, cx, &theme))
            .child(
                // Main area: left panel | center content | right runtime sidebar.
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .gap(px(6.))
                    .p(px(6.))
                    .child(left_panel(&theme))
                    .child(center_content(&theme))
                    .child(runtime_sidebar(self, cx, &theme)),
            );

        if self.output_visible {
            root = root.child(output_panel(self, &theme));
        }
        root.child(memory_bar(self, &theme))
            .child(status_strip(self, &theme))
    }
}

fn action_bar(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    let file_label = app
        .active_file
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "no file".to_string());

    let can_run = app.can_run();
    let build_label = if app.building { "Building…" } else { "Build" }.to_string();
    let run_label = if app.running { "Running…" } else { "Run" }.to_string();
    let ai_active = matches!(app.ai_status.state, AiState::Ready);
    let ai_label = match app.ai_status.state {
        AiState::Ready => format!("AI ● {}", app.ai_status.detail),
        AiState::Starting => "AI … Starting".to_string(),
        AiState::Error => format!("AI ⚠ {}", app.ai_status.detail),
        AiState::Disabled => "AI ○ Off".to_string(),
    };

    // Left toggles: Terminal now toggles the output panel; the rest stay
    // placeholders until Phase-4 wires the file tree / flow / runtime panels.
    let toggles = div()
        .flex()
        .items_center()
        .gap_2()
        .child(chip("Files", theme, false))
        .child(action_chip(
            "tgl-terminal",
            "Terminal".to_string(),
            theme,
            app.output_visible,
            false,
            cx,
            |a, _| a.action_toggle_output(),
        ))
        .child(chip("Flow", theme, false))
        .child(chip("Runtime", theme, false));

    let right_group = div()
        .flex()
        .items_center()
        .gap_2()
        // ASM viewer is a Phase-4 overlay — disabled placeholder for now.
        .child(chip("ASM", theme, false))
        .child(action_chip(
            "btn-build",
            build_label,
            theme,
            app.building,
            app.building,
            cx,
            |a, _| a.action_build(),
        ))
        .child(action_chip(
            "btn-run",
            run_label,
            theme,
            app.running,
            app.running || !can_run,
            cx,
            |a, _| a.action_run(),
        ))
        .child(action_chip(
            "btn-debug",
            "Debug".to_string(),
            theme,
            app.debugging,
            app.building,
            cx,
            |a, _| a.action_debug(),
        ))
        .child(action_chip(
            "btn-stop",
            "Stop".to_string(),
            theme,
            false,
            false,
            cx,
            |a, _| a.action_stop(),
        ))
        .child(action_chip(
            "btn-ai", ai_label, theme, ai_active, false, cx, |a, _| a.action_ai(),
        ))
        .child(action_chip(
            "btn-theme",
            "Theme".to_string(),
            theme,
            false,
            false,
            cx,
            |a, _| a.action_theme(),
        ));

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(38.))
        .pl(px(80.)) // clear the traffic lights (hiddenInset title bar)
        .pr(px(12.))
        .bg(rgb(theme.panel))
        .border_b_1()
        .border_color(rgb(theme.border))
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().text_color(rgb(theme.accent)).child("Jade"))
                .child(toggles)
                .child(
                    div()
                        .text_color(rgb(theme.muted))
                        .text_xs()
                        .child(file_label),
                ),
        )
        .child(right_group)
}

fn chip(label: &str, theme: &Theme, accent: bool) -> impl IntoElement {
    let mut el = div()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .bg(rgb(theme.bg))
        .text_color(rgb(theme.muted));
    if accent {
        el = el.text_color(rgb(theme.accent));
    }
    el.child(label.to_string())
}

/// A clickable action-bar chip. `active` accents the label; `disabled` mutes it
/// and drops the click handler. `f` is the same `action_*` method the smoke hook
/// calls, so buttons and headless verification exercise one code path.
fn action_chip(
    id: &'static str,
    label: String,
    theme: &Theme,
    active: bool,
    disabled: bool,
    cx: &mut Context<JadeApp>,
    f: impl Fn(&mut JadeApp, &mut Context<JadeApp>) + 'static,
) -> impl IntoElement {
    let color = if disabled {
        theme.muted
    } else if active {
        theme.accent
    } else {
        theme.text
    };
    let el = div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .bg(rgb(theme.bg))
        .text_color(rgb(color))
        .child(label);
    if disabled {
        el
    } else {
        el.cursor_pointer().on_click(cx.listener(move |app, _ev, _win, cx| {
            f(app, cx);
            cx.notify();
        }))
    }
}

fn left_panel(theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .w(px(260.))
        .p(px(10.))
        .rounded_lg()
        .bg(rgb(theme.panel))
        .border_1()
        .border_color(rgb(theme.border))
        .child(div().text_color(rgb(theme.muted)).text_xs().child("FILES"))
        .child(
            div()
                .text_color(rgb(theme.muted))
                .text_xs()
                .child("Open a folder to get started"),
        )
}

fn center_content(theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .rounded_lg()
        .bg(rgb(theme.bg))
        .border_1()
        .border_color(rgb(theme.border))
        .child(
            div()
                .text_color(rgb(theme.muted))
                .child("editor — center content placeholder"),
        )
}

fn runtime_sidebar(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    div()
        .id("runtime-sidebar")
        .flex()
        .flex_col()
        .gap_3()
        .w(px(280.))
        .p(px(10.))
        .rounded_lg()
        .bg(rgb(theme.panel))
        .border_1()
        .border_color(rgb(theme.border))
        .overflow_y_scroll()
        .child(training_view::render(app, cx))
        .child(telemetry_sidebar::render(app, cx))
}

/// Bottom output panel (deliverable §4): capped scrollback, monospace, muted,
/// newest visible (render the tail that fits). ANSI already stripped on ingest.
fn output_panel(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    // Render the last ~200 lines — enough to fill the strip without a huge tree.
    let start = app.output.len().saturating_sub(200);
    let mut list = div().flex().flex_col();
    for line in &app.output[start..] {
        let text = if line.is_empty() { " ".to_string() } else { line.clone() };
        list = list.child(
            div()
                .text_color(rgb(theme.muted))
                .text_xs()
                .child(text),
        );
    }
    div()
        .id("output-panel")
        .h(px(200.))
        .w_full()
        .p(px(8.))
        .bg(rgb(theme.bg))
        .border_t_1()
        .border_color(rgb(theme.border))
        .overflow_y_scroll()
        .child(list)
}

/// Bottom memory-bar strip (deliverable §6): SYS MEM · HEAP · PEAK · PRESSURE ·
/// CPU · GPU, colored by threshold classification.
fn memory_bar(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let v = project(&app.mem, &app.sys_stats);
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(24.))
        .px(px(10.))
        .bg(rgb(theme.panel))
        .border_t_1()
        .border_color(rgb(theme.border))
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(metric("SYS MEM", v.sys_mem, Level::Normal, theme))
                .child(metric("HEAP", v.heap, v.heap_level, theme))
                .child(metric("PEAK", v.peak, v.peak_level, theme))
                .child(metric("PRESSURE", v.pressure_dots, v.pressure_level, theme)),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(metric("CPU", v.cpu, v.cpu_level, theme))
                .child(metric("GPU", v.gpu, v.gpu_level, theme)),
        )
}

fn metric(label: &str, value: String, level: Level, theme: &Theme) -> impl IntoElement {
    let vc = match level {
        Level::Normal => theme.text,
        Level::Warn => theme.amber,
        Level::Danger => theme.red,
    };
    div()
        .flex()
        .items_center()
        .gap_1()
        .text_xs()
        .child(div().text_color(rgb(theme.muted)).child(label.to_string()))
        .child(div().text_color(rgb(vc)).child(value))
}

fn status_strip(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    let mut text = format!(
        "socket {}   ·   scalars {}   timings {}   tensors {}",
        app.server.socket_path().display(),
        app.scalars_seen,
        app.timings_seen,
        app.tensors_seen
    );
    if let Some(r) = &app.last_run {
        text.push_str(&format!(
            "   ·   last run exit {} · {}ms · {} lines",
            r.exit_code, r.duration_ms, r.executed_lines
        ));
    }
    div()
        .flex()
        .items_center()
        .h(px(22.))
        .px(px(10.))
        .bg(rgb(theme.panel))
        .border_t_1()
        .border_color(rgb(theme.border))
        .child(div().text_color(rgb(theme.muted)).text_xs().child(text))
}
