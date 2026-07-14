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
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_ai::{AiState, AiStatus, InlineCompletionBackend};
use forge_build::{
    BuildEngine, BuildResult, CompileRequest, RunConfig, RunEvent, RunResult, PROBE_DYLIB,
};
use forge_debug::{DebugEvent, LldbDriver};
use forge_sysmon::{SystemMonitor, SystemStats};
use forge_telemetry::{Event, Kind, TelemetryServer};
use forge_term::{GridSnapshot, TermEvent, TermId, TermManager};
use gpui::{div, prelude::*, px, rgb, Context, FocusHandle, Window};
use serde_json::{Map, Value};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

use crate::editor_view::EditorState;
use crate::highlight::TokenPalette;
use crate::memory_bar::{project, Level, MemoryBarState};
use crate::output::push_output;
use crate::panels::runtime_panel::{self, RunRecord};
use crate::panels::{code_view, file_tree, telemetry_sidebar, terminal_panel, training_view};
use crate::prefs::TelemetryPrefs;
use crate::registry::{key_of, TelemetryRegistry, DEFAULT_MAX_DIM};
use crate::theme::Theme;
use crate::training::{TensorFrame, TrainingData};
use crate::wg3d::WeightGrid3D;
use crate::workspace_tree::FileTree;

/// Which view the bottom panel shows. The TERMINAL view is a live shell; the
/// OUTPUT view is the plain `[forge]`/build/run scrollback fallback (see
/// `terminal_panel` for why status lines can't be injected into the shell grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomView {
    Terminal,
    Output,
}

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
    /// A terminal event from `forge-term` (grid damaged → re-snapshot; child
    /// exited → dim `[exited <code>]`). Phase-4 wave 2, §5.2.
    Term(TermEvent),
    /// The workspace file tree changed on disk (debounced fs-watch, §5.1) — the
    /// app re-scans preserving expansion. Carries nothing; the refresh reads disk.
    TreeChanged,
}

/// Everything [`JadeApp`] needs from `main` to be constructed — kept as one
/// struct so the GUI constructor and the headless smoke path share wiring.
pub struct AppDeps {
    pub server: Arc<TelemetryServer>,
    pub engine: Arc<BuildEngine>,
    pub ai: Arc<InlineCompletionBackend>,
    pub sysmon: Arc<SystemMonitor>,
    /// Headless terminal engine (bottom TERMINAL strip, §5.2). Shared behind an
    /// `Arc`; its events are forwarded onto [`AppEvent::Term`] in `main.rs`.
    pub term: Arc<TermManager>,
    /// Handle to the tokio runtime; button handlers spawn engine futures here.
    pub runtime: Handle,
    /// The write end of the unified event channel — cloned into every future.
    pub app_tx: UnboundedSender<AppEvent>,
    pub active_file: Option<PathBuf>,
    pub repo_root: PathBuf,
    /// Root the file-tree panel scans (the `--project` dir, else the active
    /// file's parent, else the repo root).
    pub workspace_root: PathBuf,
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
    /// 3D weight-grid overlay (§7.2). Owns its own 64-frame ring per buffer,
    /// fed from the telemetry apply path below and rendered only while visible.
    pub wg3d: WeightGrid3D,

    // ── Phase-3 engine handles ────────────────────────────────────────────────
    engine: Arc<BuildEngine>,
    ai: Arc<InlineCompletionBackend>,
    sysmon: Arc<SystemMonitor>,
    runtime: Handle,
    app_tx: UnboundedSender<AppEvent>,
    repo_root: PathBuf,
    /// Root the file tree scans + the cwd new terminals spawn in.
    workspace_root: PathBuf,
    /// LLDB driver, constructed lazily on the first Debug (deliverable §2).
    driver: Option<Arc<AsyncMutex<LldbDriver>>>,

    // ── Terminal panel (§5.2) ─────────────────────────────────────────────────
    /// Shared terminal engine (also held in `AppDeps` for the event forwarder).
    pub term: Arc<TermManager>,
    /// The single visible terminal instance, created on first show.
    pub term_id: Option<TermId>,
    /// Latest grid snapshot (refreshed on `Damaged`; rendered by the panel).
    pub term_snapshot: Option<GridSnapshot>,
    /// Set once the child exits — renders the dim `[exited <code>]` line.
    pub term_exited: bool,
    pub term_exit_code: Option<i32>,
    /// True if PTY allocation failed — stop retrying, show a message.
    term_failed: bool,
    /// Last cols/rows applied by the resize canvas (packed `cols<<16 | rows`),
    /// so we only `resize()` when the derived geometry actually changed.
    pub term_last_size: Arc<AtomicU32>,
    /// Focus handle for the terminal (created lazily in render; None headless).
    pub term_focus: Option<FocusHandle>,
    /// Which view the bottom panel shows (TERMINAL vs OUTPUT scrollback).
    pub bottom_view: BottomView,

    // ── Code-viewing vertical (deliverables §1-§5) ────────────────────────────
    /// Scanned workspace file tree (left panel). `None` if the root is unreadable.
    pub tree: Option<FileTree>,
    /// Open tabs + read-only highlighted viewer state (center).
    pub editor: EditorState,

    // ── Build/run/debug lifecycle state ───────────────────────────────────────
    pub active_file: Option<PathBuf>,
    pub building: bool,
    pub running: bool,
    pub debugging: bool,
    pub last_build: Option<BuildResult>,
    last_sanitize: bool,
    last_instrument: bool,
    pub last_run: Option<RunStatus>,

    // ── Runtime panel state (§5.4) ────────────────────────────────────────────
    /// Whether the RUNTIME panel is shown (toggled by the Runtime chip).
    pub runtime_visible: bool,
    /// Wall-clock start of the in-flight run (drives the live SPEED tick).
    pub run_started: Option<Instant>,
    /// 1-based counter of completed runs.
    pub run_counter: usize,
    /// Last 10 completed runs (most recent last), for HISTORY.
    pub run_history: Vec<RunRecord>,
    /// Fastest completed run so far (ms).
    pub best_run_ms: Option<u128>,
    /// True when the most recent run beat the previous best.
    pub last_was_best: bool,
    /// Signed delta (ms) of the last run vs the run before it.
    pub last_delta_ms: Option<i128>,
    /// The last run's per-line execution counts (HOTSPOTS source).
    pub last_executed: HashMap<u32, u32>,

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
        // Scan the workspace root for the file tree (§5.1) and seed the editor.
        let tree = Some(FileTree::scan(deps.workspace_root.clone()));
        let mut editor = EditorState::new(TokenPalette::forge_dark());
        // Preserve the --file/--project seeding as the initially open tab: open it
        // through the real tab/highlight path so `active_file` follows the tab.
        let mut active_file = deps.active_file.clone();
        if let Some(file) = &deps.active_file {
            if editor.open(file).is_ok() {
                active_file = editor.active_path();
            }
        }

        Self {
            server: deps.server,
            registry: TelemetryRegistry::new(),
            training: TrainingData::new(),
            prefs: TelemetryPrefs::load(),
            theme: Theme::forge_dark(),
            wg3d: WeightGrid3D::new(),

            engine: deps.engine,
            ai: deps.ai,
            sysmon: deps.sysmon,
            runtime: deps.runtime,
            app_tx: deps.app_tx,
            repo_root: deps.repo_root,
            workspace_root: deps.workspace_root,
            driver: None,

            term: deps.term,
            term_id: None,
            term_snapshot: None,
            term_exited: false,
            term_exit_code: None,
            term_failed: false,
            term_last_size: Arc::new(AtomicU32::new(0)),
            term_focus: None,
            bottom_view: BottomView::Terminal,

            tree,
            editor,

            active_file,
            building: false,
            running: false,
            debugging: false,
            last_build: None,
            last_sanitize: false,
            last_instrument: false,
            last_run: None,

            runtime_visible: false,
            run_started: None,
            run_counter: 0,
            run_history: Vec::new(),
            best_run_ms: None,
            last_was_best: false,
            last_delta_ms: None,
            last_executed: HashMap::new(),

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
            AppEvent::Term(ev) => self.on_term_event(ev),
            AppEvent::TreeChanged => {
                if let Some(tree) = &mut self.tree {
                    tree.refresh();
                }
            }
        }
    }

    /// Follow the `forge-term` contract: on `Damaged` re-snapshot the current
    /// instance; on `Exited` record the code for the dim `[exited …]` line.
    fn on_term_event(&mut self, ev: TermEvent) {
        match ev {
            TermEvent::Damaged { id } => {
                if self.term_id == Some(id) {
                    self.term_snapshot = self.term.snapshot(id);
                }
            }
            TermEvent::Exited { id, code } => {
                if self.term_id == Some(id) {
                    self.term_exited = true;
                    self.term_exit_code = code;
                    // Capture the final grid so the last output stays visible.
                    self.term_snapshot = self.term.snapshot(id);
                }
            }
        }
    }

    /// Create the single terminal instance on first show (cwd = workspace root).
    /// Degrades gracefully if the PTY can't be allocated (§5.2).
    fn ensure_terminal(&mut self) {
        if self.term_id.is_some() || self.term_failed {
            return;
        }
        match self.term.create(&self.workspace_root) {
            Ok(id) => {
                self.term_id = Some(id);
                self.term_exited = false;
                self.term_exit_code = None;
                self.term_last_size.store(0, std::sync::atomic::Ordering::Relaxed);
                self.term_snapshot = self.term.snapshot(id);
            }
            Err(e) => {
                self.term_failed = true;
                self.status_line(&format!("[forge] terminal unavailable: {e}"));
            }
        }
    }

    /// New-terminal button: replace the single visible instance with a fresh
    /// shell (the panel shows one terminal at a time; §5.2's multi-instance list
    /// is deferred).
    pub fn action_new_terminal(&mut self) {
        if let Some(old) = self.term_id.take() {
            self.term.destroy(old);
        }
        self.term_snapshot = None;
        self.term_exited = false;
        self.term_exit_code = None;
        self.term_failed = false;
        self.term_last_size.store(0, std::sync::atomic::Ordering::Relaxed);
        self.bottom_view = BottomView::Terminal;
        self.output_visible = true;
        self.ensure_terminal();
    }

    /// Toggle the RUNTIME panel (Runtime chip, §5.4).
    pub fn action_toggle_runtime(&mut self) {
        self.runtime_visible = !self.runtime_visible;
    }

    /// Switch the bottom panel between the live TERMINAL and the OUTPUT
    /// scrollback fallback.
    pub fn set_bottom_view(&mut self, view: BottomView) {
        self.bottom_view = view;
        self.output_visible = true;
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
        self.run_started = None;
        let ms = res.duration.as_millis();
        let executed = res.executed_lines.len();

        // ── Runtime panel bookkeeping (§5.4) ──
        // vs-last delta is measured against the previous run (before we push).
        self.last_delta_ms = self
            .run_history
            .last()
            .map(|prev| ms as i128 - prev.duration_ms as i128);
        // Personal best: only "beaten" if there was a slower prior best.
        self.last_was_best = self.best_run_ms.map(|b| ms < b).unwrap_or(false);
        self.best_run_ms = Some(self.best_run_ms.map_or(ms, |b| b.min(ms)));
        self.run_counter += 1;
        self.run_history.push(RunRecord {
            n: self.run_counter,
            duration_ms: ms,
            peak: self.mem.peak_allocation,
        });
        // Keep memory bounded; HISTORY only renders the last 10 anyway.
        if self.run_history.len() > 50 {
            let drop = self.run_history.len() - 50;
            self.run_history.drain(0..drop);
        }
        self.last_executed = res.executed_lines.clone();
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
        self.run_started = Some(Instant::now()); // drive the live SPEED tick
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

    // ── Code-viewing vertical (file tree + tabs + viewer) ─────────────────────

    /// Open a file in the editor (deliverable §3): reads + highlights it once
    /// (deduped by path), makes it the active tab, and points `active_file` at it
    /// so the Build/Run target follows the front tab.
    pub fn open_file(&mut self, path: PathBuf) {
        match self.editor.open(&path) {
            Ok(_) => self.active_file = self.editor.active_path(),
            Err(e) => self.status_line(&format!("[forge] Could not open {}: {e}", path.display())),
        }
    }

    /// Toggle a directory in the file tree (deliverable §2): lazily loads its
    /// children on first expansion.
    pub fn toggle_dir(&mut self, path: PathBuf) {
        if let Some(tree) = &mut self.tree {
            tree.toggle_dir(&path);
        }
    }

    /// Switch the active tab; `active_file` follows.
    pub fn switch_tab(&mut self, index: usize) {
        self.editor.switch(index);
        self.active_file = self.editor.active_path();
    }

    /// Close a tab (close-index logic per `editor-manager.ts:208-241`);
    /// `active_file` follows the new active tab (or clears when none remain).
    pub fn close_tab(&mut self, index: usize) {
        self.editor.close(index);
        self.active_file = self.editor.active_path();
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
                let frame = TensorFrame {
                    step,
                    rows,
                    cols,
                    src_rows,
                    src_cols,
                    data,
                };
                // §7.2 event feed: the 3D grid keeps its OWN 64-frame ring per
                // buffer, filled from the same apply path even while hidden.
                self.wg3d.on_frame(&name, frame.clone());
                self.training.push_tensor(&name, frame);
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();

        // Create the terminal (and its focus handle) on first show of the strip.
        if self.output_visible && self.bottom_view == BottomView::Terminal {
            self.ensure_terminal();
        }
        let term_handle = self
            .term_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();

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
                    .child(left_panel(self, cx, &theme))
                    .child(center_content(self, cx, &theme))
                    .child(runtime_sidebar(self, cx, &theme)),
            );

        if self.output_visible {
            root = root.child(bottom_panel(self, cx, &theme, term_handle));
        }
        let mut root = root
            .child(memory_bar(self, &theme))
            .child(status_strip(self, &theme));

        // §7.2 open/close hook: while visible, overlay the full-window 3D grid
        // on top of everything and hand it keyboard focus (for Esc).
        if self.wg3d.visible {
            let focus = crate::wg3d::render::ensure_focus(self, cx);
            if !focus.is_focused(window) {
                focus.focus(window, cx);
            }
            let vp = window.viewport_size();
            let overlay = crate::wg3d::render::overlay(
                self,
                focus,
                f32::from(vp.width),
                f32::from(vp.height),
                cx,
            );
            root = root.child(overlay);
        }
        root
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
    let terminal_active = app.output_visible && app.bottom_view == BottomView::Terminal;
    let toggles = div()
        .flex()
        .items_center()
        .gap_2()
        .child(chip("Files", theme, false))
        .child(action_chip(
            "tgl-terminal",
            "Terminal".to_string(),
            theme,
            terminal_active,
            false,
            cx,
            |a, _| {
                // Show + focus the TERMINAL view, or hide the strip if it's the
                // one already up.
                if a.output_visible && a.bottom_view == BottomView::Terminal {
                    a.action_toggle_output();
                } else {
                    a.set_bottom_view(BottomView::Terminal);
                }
            },
        ))
        .child(chip("Flow", theme, false))
        .child(action_chip(
            "tgl-runtime",
            "Runtime".to_string(),
            theme,
            app.runtime_visible,
            false,
            cx,
            |a, _| a.action_toggle_runtime(),
        ));

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

/// Left panel: the file-tree card (deliverable §2), replacing the placeholder.
fn left_panel(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
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
        .overflow_hidden()
        .child(file_tree::render(app, cx))
}

/// Center: the tab strip + read-only code viewer (deliverables §3, §5), replacing
/// the placeholder.
fn center_content(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .rounded_lg()
        .bg(rgb(theme.bg))
        .border_1()
        .border_color(rgb(theme.border))
        .overflow_hidden()
        .child(code_view::tab_strip(app, cx, theme))
        .child(code_view::render(app, cx))
}

fn runtime_sidebar(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    let mut col = div()
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
        .overflow_y_scroll();
    // RUNTIME panel sits above TRAINING, shown when toggled (§5.4).
    if app.runtime_visible {
        col = col.child(runtime_panel::render(app, cx));
    }
    col.child(training_view::render(app, cx))
        .child(telemetry_sidebar::render(app, cx))
}

/// Bottom panel (§5.2): a header (view toggle · new-terminal · minimize) over the
/// live TERMINAL grid or the OUTPUT scrollback fallback. `[forge]`/build/run
/// status lines land in OUTPUT (the terminal is a real shell we can't inject
/// display text into — see `terminal_panel`).
fn bottom_panel(
    app: &JadeApp,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
    term_handle: FocusHandle,
) -> impl IntoElement {
    let is_term = app.bottom_view == BottomView::Terminal;

    // View-toggle tabs: TERMINAL | OUTPUT.
    let view_tab = |id: &'static str, label: &'static str, active: bool, view: BottomView| {
        let color = if active { theme.text } else { theme.muted };
        div()
            .id(id)
            .px_2()
            .text_xs()
            .cursor_pointer()
            .text_color(rgb(color))
            .on_click(cx.listener(move |a: &mut JadeApp, _ev, _win, cx| {
                a.set_bottom_view(view);
                cx.notify();
            }))
            .child(label)
    };

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(22.))
        .px(px(8.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(view_tab("bv-terminal", "TERMINAL", is_term, BottomView::Terminal))
                .child(view_tab("bv-output", "OUTPUT", !is_term, BottomView::Output)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                // New-terminal.
                .child(
                    div()
                        .id("term-new")
                        .text_color(rgb(theme.muted))
                        .text_xs()
                        .cursor_pointer()
                        .on_click(cx.listener(|a: &mut JadeApp, _ev, _win, cx| {
                            a.action_new_terminal();
                            cx.notify();
                        }))
                        .child("+"),
                )
                // Minimize (hide the strip).
                .child(
                    div()
                        .id("term-min")
                        .text_color(rgb(theme.muted))
                        .text_xs()
                        .cursor_pointer()
                        .on_click(cx.listener(|a: &mut JadeApp, _ev, _win, cx| {
                            a.action_toggle_output();
                            cx.notify();
                        }))
                        .child("—"),
                ),
        );

    let body = if is_term {
        div()
            .flex()
            .flex_1()
            .w_full()
            .child(terminal_panel::render(app, term_handle, cx))
            .into_any_element()
    } else {
        output_view(app, theme).into_any_element()
    };

    div()
        .id("bottom-panel")
        .flex()
        .flex_col()
        .h(px(220.))
        .w_full()
        .bg(rgb(theme.bg))
        .border_t_1()
        .border_color(rgb(theme.border))
        .child(header)
        .child(body)
}

/// The OUTPUT scrollback view (deliverable §4): capped scrollback, monospace,
/// muted, newest visible. ANSI already stripped on ingest.
fn output_view(app: &JadeApp, theme: &Theme) -> impl IntoElement {
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
        .flex_1()
        .w_full()
        .p(px(8.))
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
