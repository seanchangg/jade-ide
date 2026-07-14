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
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_ai::{AiState, AiStatus, InfillRequest, InlineCompletionBackend};
use forge_build::{
    AsmResult, BuildEngine, BuildResult, CompileRequest, RunConfig, RunEvent, RunResult, PROBE_DYLIB,
};
use forge_buffer::{Point, Selection};
use forge_debug::{DebugEvent, LldbDriver, LocalVariable};
use forge_lsp::{
    CompletionItem, DidChange, HoverContents, LspClient, LspEvent, LspHandle, TextDocumentSyncKind,
};
use forge_sysmon::{SystemMonitor, SystemStats};
use forge_telemetry::{Event, Kind, TelemetryServer};
use forge_term::{GridSnapshot, TermEvent, TermId, TermManager};
use gpui::{
    div, hsla, prelude::*, px, rgb, Bounds, BoxShadow, ClipboardItem, Context,
    EntityInputHandler, FocusHandle, KeyDownEvent, MouseButton, PathPromptOptions, Pixels,
    UTF16Selection, Window, WindowControlArea,
};
use serde_json::{Map, Value};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

use crate::editor_view::{self, EditorState};
use crate::highlight::TokenPalette;
use crate::memory_bar::{project, Level, MemoryBarState};
use crate::output::push_output;
use crate::panels::runtime_panel::{self, RunRecord};
use crate::panels::{
    asm_view, code_view, debug_panel, file_tree, structure_panel, telemetry_sidebar,
    terminal_panel, training_view,
};
use crate::prefs::TelemetryPrefs;
use crate::quick_open::{self, FileEntry, KeyAction, Match, QuickOpenState};
use crate::registry::{key_of, TelemetryRegistry, DEFAULT_MAX_DIM};
use crate::structure::Symbol;
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

/// Which view the left sidebar shows (§5.5): the FILES tree or the STRUCTURE
/// outline. Toggled by the sidebar tab switcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
    Files,
    Structure,
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
    /// clangd finished its initialize handshake; carries the live handle + the
    /// negotiated sync kind so the app can start `didOpen`-ing tabs (E2).
    LspReady {
        handle: Arc<LspHandle>,
        sync_kind: TextDocumentSyncKind,
    },
    /// A clangd event: `Ready` / `Diagnostics{path,…}` / `Exited` (E2).
    Lsp(LspEvent),
    /// A completion response arrived for request `generation`, anchored at the
    /// caret point it was requested from (E2). Stale generations are dropped.
    Completion {
        generation: u64,
        items: Vec<CompletionItem>,
        anchor: (usize, usize),
    },
    /// A hover response for request `generation` at `(row,col)` (E2).
    Hover {
        generation: u64,
        text: Option<String>,
        row: usize,
        col: usize,
    },
    /// A go-to-definition target resolved from a ⌘-click (E2): open + reveal.
    Definition { path: PathBuf, line: usize },
    /// An AI ghost-text (`/infill`) response for request `generation` (§4.11).
    /// Carries the raw model `content` (`None` on any failure/abort) plus the
    /// `(prefix, suffix, line_suffix, anchor, max_lines)` the request was made
    /// with, so the app can cache the raw output and post-process it on arrival.
    Ghost {
        generation: u64,
        content: Option<String>,
        prefix: String,
        suffix: String,
        line_suffix: String,
        anchor: (usize, usize),
        max_lines: usize,
    },
    /// An `-O3 -march=native` assembly listing for request `generation` (§6 ASM
    /// viewer). Carries the engine result (asm text + `.loc` line map). Stale
    /// generations (superseded by a newer edit-refresh) are dropped.
    AsmReady { generation: u64, result: AsmResult },
    /// Lazily-fetched children of an expandable debug variable (§5.8): the lldb
    /// expression `path` the fetch was keyed on plus the resolved `children`.
    VarChildren { path: String, children: Vec<LocalVariable> },
}

/// Spawns (or re-spawns) the debounced fs-watch on a root, returning an opaque
/// keep-alive guard (the `notify` watcher, type-erased) that must be held for the
/// watch to stay live; dropping it stops the watch. `main` supplies the real
/// implementation (capturing the tokio Handle + unified sender); tests pass a
/// no-op. Owned by [`JadeApp`] so [`open_project`](JadeApp::open_project) can
/// restart the watch on a newly-opened folder (§5.1).
pub type FsWatchSpawn = Arc<dyn Fn(&Path) -> Option<Box<dyn Send>> + Send + Sync>;

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
    /// Whether a workspace is open at launch (inventory §2). When false the
    /// welcome overlay covers the editor and the tree does not scan the root.
    pub workspace_opened: bool,
    /// Re-spawnable fs-watch (owned by `JadeApp`; restarted by `open_project`).
    pub fs_watch: FsWatchSpawn,
    pub demo: bool,
}

/// Recorded stats from the last completed run, shown in the status area.
#[derive(Clone)]
pub struct RunStatus {
    pub exit_code: i32,
    pub duration_ms: u128,
    pub executed_lines: usize,
}

/// Live autocomplete popup state (E2). Filtered against what's typed; `selected`
/// indexes into `filtered`, which indexes into `items`.
pub struct CompletionState {
    pub items: Vec<CompletionItem>,
    pub filtered: Vec<usize>,
    pub selected: usize,
    /// Caret point (row, char col) the popup is anchored under.
    pub anchor: (usize, usize),
}

impl CompletionState {
    /// The currently highlighted item, if any.
    pub fn current(&self) -> Option<&CompletionItem> {
        self.filtered.get(self.selected).map(|&i| &self.items[i])
    }
}

/// Floating hover-panel state (E2): plain-text contents at a screen anchor.
pub struct HoverState {
    pub text: String,
    pub row: usize,
    pub col: usize,
}

/// The current AI ghost-text suggestion (§4.11): the (possibly multi-line)
/// completion text and the caret `(row, col)` it is anchored at.
pub struct GhostState {
    pub text: String,
    pub anchor: (usize, usize),
}

/// An in-flight inline benchmark-name input (§5.4). The buffer is prefilled with
/// `#<run> <flags>` and committed on Enter (Esc cancels). `run_index` picks the
/// HISTORY row's run + its recorded stats; `flags` are carried onto the entry.
pub struct BenchNaming {
    pub run_index: usize,
    pub buffer: String,
    pub flags: String,
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
    /// Whether a workspace is open (inventory §2). While false the welcome
    /// overlay covers the editor area and the tree is empty; `open_project` flips
    /// it true.
    pub workspace_opened: bool,
    /// Re-spawn hook for the fs-watch; called on startup + by `open_project`.
    fs_watch: FsWatchSpawn,
    /// Live fs-watch keep-alive guard (the type-erased `notify` watcher). Held so
    /// the watch stays up; replaced (old dropped first) when a folder is opened.
    fs_watcher: Option<Box<dyn Send>>,
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
    /// Open tabs + editable buffer-backed editor state (center).
    pub editor: EditorState,

    // ── Editable editor surface (E2) ──────────────────────────────────────────
    /// Focus handle for the editor surface (created lazily; None headless).
    pub editor_focus: Option<FocusHandle>,
    /// True while a left-drag is extending the selection.
    pub editor_selecting: bool,
    /// Set by [`open_file`](Self::open_file); the next render focuses the editor
    /// surface (open sites don't have a `Window`, render does). Without this the
    /// editor starts life unfocused — no caret, dead arrow keys — until the user
    /// happens to click inside the code area.
    pub pending_editor_focus: bool,
    /// The code-text left edge in window px (f32 bits), captured each frame by a
    /// canvas underlay, so mouse handlers can map click-x → char column.
    pub editor_text_left: Arc<AtomicU32>,
    /// The mono font's real char advance at the editor size (f32 bits), measured
    /// from the text system each frame by the same canvas. Falls back to the
    /// Menlo-13 constant until first paint. Keeping this measured (not hardcoded)
    /// means caret placement and click→column mapping survive a font swap
    /// (bundled JetBrains Mono vs Menlo).
    pub editor_char_w: Arc<AtomicU32>,
    /// Caret blink (530ms phase, Electron/macOS-style). `caret_blink_show`
    /// is the current phase; any caret activity forces it visible and stamps
    /// `caret_last_active` so the caret holds steady-on while typing/moving.
    pub caret_blink_show: bool,
    /// `now_ms()` of the last caret movement/edit.
    pub caret_last_active: u64,
    /// True once the 530ms blink-driver task was spawned (render, GUI only).
    blink_task_running: bool,
    /// Visible editor row count (viewport height / line height), captured each
    /// frame, so `scroll_caret_into_view` does minimal follow-scroll.
    pub editor_rows: Arc<AtomicU32>,
    /// Monotonic clock origin for the decoration debounces + IME timing.
    epoch: Instant,
    /// True while the decoration-recompute wake task is running (avoids dupes).
    decoration_wake_running: bool,

    // ── LSP (clangd) integration (E2) ─────────────────────────────────────────
    /// Live clangd handle once initialized (`did_*` notifications go through it).
    lsp: Option<Arc<LspHandle>>,
    /// The sync kind clangd negotiated (Incremental vs Full didChange).
    lsp_sync_kind: TextDocumentSyncKind,
    /// True once initialize has been kicked off (so we do it once per workspace).
    lsp_init_started: bool,
    /// The forge include dir passed to clangd as a fallback `-I`, if it exists.
    lsp_include: Option<PathBuf>,
    /// Autocomplete popup (E2). `Some` when visible.
    pub completion: Option<CompletionState>,
    /// Monotonic completion-request generation (supersede stale responses).
    completion_gen: u64,
    /// Hover panel (E2). `Some` when a dwell resolved hover contents.
    pub hover: Option<HoverState>,
    /// Monotonic hover-request generation (supersede stale dwell responses).
    hover_gen: u64,
    /// The (row,col) the last hover request targeted, so mouse-move doesn't
    /// re-request while the pointer sits on the same cell.
    hover_target: Option<(usize, usize)>,

    // ── AI ghost text / editor extras (Phase-3b E3) ───────────────────────────
    /// Whether ghost text is offered (workspace UI toggle `aiCompletionEnabled`,
    /// §4.11). Distinct from the backend being Ready — both must hold.
    pub ai_completion_enabled: bool,
    /// Multiline ghost mode (`aiMultiline`, §4.11): 6-line vs single-line.
    pub ai_multiline: bool,
    /// The current ghost suggestion, if any.
    pub ghost: Option<GhostState>,
    /// Monotonic ghost-request generation (supersede stale `/infill` responses).
    ghost_gen: u64,
    /// 48-entry raw-suggestion cache with typed-through hits (§4.11).
    ghost_cache: crate::ghost::GhostCache,

    // ── XP bar (§4.10) ────────────────────────────────────────────────────────
    pub xp: crate::xp::XpState,
    /// Global `xpTotal` persistence (`~/.config/jade/`, Electron-migrating).
    xp_store: crate::xp::XpStore,

    // ── Structure panel + Quick Open (Phase-4 wave 3, §5.5/§5.7) ──────────────
    /// Which view the left sidebar shows (FILES tree vs STRUCTURE outline).
    pub sidebar_tab: SidebarTab,
    /// Whether the left sidebar is collapsed to the 28px strip (§2; app.ts:449-459,
    /// main.css:481-495). Binary toggle — 260px ⇄ 28px "FILES" strip.
    pub sidebar_collapsed: bool,
    /// Quick Open overlay state (`Some` == open); the transient query + selection.
    pub quick_open: Option<QuickOpenState>,
    /// Focus handle for the Quick Open overlay (created lazily; None headless).
    quick_open_focus: Option<FocusHandle>,
    /// Cached flattened file list for Quick Open, per workspace root. Rebuilt on
    /// first ⌘P and invalidated by `TreeChanged` (§5.7 "cached file list").
    file_cache: Option<Vec<FileEntry>>,

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
    /// The last run's per-line execution counts (HOTSPOTS source; also the exec
    /// annotation counts, §4.5 system 2).
    pub last_executed: HashMap<u32, u32>,
    /// The *previous* run's per-line counts — the snapshot the exec annotations
    /// diff against for their ↑/↓ arrows. Rotated in [`Self::on_run_done`]
    /// (`memory-decorations.ts:142-153`).
    pub prev_executed: HashMap<u32, u32>,
    /// Source line (1-based) of the last run's first error, parsed from the
    /// sanitizer output on a failing run (app.ts:1167-1172). Rendered as the red
    /// `⊘` error line by the flow decorations.
    pub error_line: Option<u32>,

    // ── Editor decorations (Phase-4 wave 3, §4.8) ─────────────────────────────
    /// Whether the execution-flow glyphs + tints are shown (the Flow chip, ⌘E).
    pub flow_visible: bool,
    /// Scroll handle for the code viewer's `uniform_list`, so Cmd+Click glyph
    /// navigation can reveal a target line.
    pub code_scroll: gpui::UniformListScrollHandle,

    // ── ASM viewer (§6, ⌘⇧A) ──────────────────────────────────────────────────
    /// Whether the right-half ASM overlay is shown.
    pub asm_visible: bool,
    /// The current assembly listing + bidirectional line map (`None` until the
    /// first generate_asm resolves for the active file).
    pub asm: Option<crate::asm::AsmView>,
    /// Monotonic asm-request generation (supersede stale / auto-refresh responses).
    asm_gen: u64,
    /// True while an asm generation is in flight (drives the "generating…" hint).
    pub asm_loading: bool,
    /// Scroll handle for the virtualized asm line list (asm→src scroll target).
    pub asm_scroll: gpui::UniformListScrollHandle,

    // ── Debug panel (§5.8) + breakpoints (§4.6) ───────────────────────────────
    /// Structural debug session (frames / variables tree / console).
    pub debug: crate::debug::DebugSession,
    /// Whether the debug panel is docked (above the terminal, hiding it).
    pub debug_visible: bool,
    /// The `output_visible` value to restore when the debug panel hides (§5.8
    /// "restoring prior visibility on hide").
    debug_term_restore: Option<bool>,
    /// Per-file breakpoint sets (gutter toggles; synced live to the driver;
    /// persisted in the `ui` blob).
    pub breakpoints: crate::debug::Breakpoints,

    // ── Benchmarks (§5.4) ─────────────────────────────────────────────────────
    /// Saved named benchmarks (persisted in the `ui` blob; sorted fastest-first
    /// at render time).
    pub benchmarks: Vec<crate::benchmark::Benchmark>,
    /// In-flight inline benchmark-name input (`Some` while naming a saved run).
    pub bench_naming: Option<BenchNaming>,
    /// Focus handle for the benchmark-name input (created lazily; None headless).
    bench_focus: Option<FocusHandle>,

    // ── Per-workspace UI persistence (§1.2) ───────────────────────────────────
    /// Monotonic ui-save generation (1500ms debounce around `workspace_state::save`).
    ui_save_gen: u64,

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
        let opened = deps.workspace_opened;
        // Scan the workspace root for the file tree (§5.1) and seed the editor —
        // but only when a workspace is open; otherwise the welcome overlay covers
        // the editor and the tree must NOT scan the fallback repo root (§2).
        let tree = opened.then(|| FileTree::scan(deps.workspace_root.clone()));
        // Start the fs-watch on the open root (owned here so `open_project` can
        // restart it). No watch until a folder is opened.
        let fs_watcher = if opened {
            (deps.fs_watch)(&deps.workspace_root)
        } else {
            None
        };
        // clangd fallback `-I<repo>/include` when that directory exists.
        let lsp_include = {
            let inc = deps.repo_root.join("include");
            inc.is_dir().then_some(inc)
        };
        // Editor extras (Phase-3b E3): the global xpTotal.
        let (xp_store, xp_total) = crate::xp::XpStore::load();

        // Per-workspace UI blob (§1.2): tabs, panel visibility, breakpoints,
        // benchmarks, ai toggle. Restored after a workspace opens. Saves
        // merge-preserve any stickyNotes the Electron app left in the file.
        let ui = crate::workspace_state::load(&deps.workspace_root);

        let mut editor = EditorState::new(TokenPalette::forge_dark());
        // Preserve the --file/--project seeding as the initially open tab: open it
        // through the real tab/highlight path so `active_file` follows the tab.
        let mut active_file = deps.active_file.clone();
        if let Some(file) = &deps.active_file {
            if editor.open(file).is_ok() {
                active_file = editor.active_path();
            }
        }
        // Restore persisted open tabs, skipping deleted files silently
        // (editor-manager.ts:272-280).
        for tab in &ui.open_tabs {
            let p = PathBuf::from(&tab.path);
            if p.is_file() {
                let _ = editor.open(&p);
            }
        }
        // Restore the active tab index (clamped), else follow the seeded file.
        if let Some(idx) = ui.active_tab_index {
            if idx >= 0 && (idx as usize) < editor.tabs.len() {
                editor.switch(idx as usize);
            }
        }
        active_file = editor.active_path().or(active_file);
        let editor_has_tab = editor.active.is_some();

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
            workspace_opened: opened,
            fs_watch: deps.fs_watch,
            fs_watcher,
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

            editor_focus: None,
            editor_selecting: false,
            // Seeded/restored tabs open before the first render — ask it to
            // focus the editor so the caret + keyboard are live from frame one.
            pending_editor_focus: editor_has_tab,
            editor_text_left: Arc::new(AtomicU32::new(0)),
            editor_char_w: Arc::new(AtomicU32::new(
                crate::panels::code_view::CHAR_W.to_bits(),
            )),
            editor_rows: Arc::new(AtomicU32::new(30)),
            caret_blink_show: true,
            caret_last_active: 0,
            blink_task_running: false,
            epoch: Instant::now(),
            decoration_wake_running: false,

            lsp: None,
            lsp_sync_kind: TextDocumentSyncKind::FULL,
            lsp_init_started: false,
            lsp_include,
            completion: None,
            completion_gen: 0,
            hover: None,
            hover_gen: 0,
            hover_target: None,

            ai_completion_enabled: ui.ai_completion_enabled.unwrap_or(true),
            ai_multiline: false,
            ghost: None,
            ghost_gen: 0,
            ghost_cache: crate::ghost::GhostCache::new(),

            xp: crate::xp::XpState::new(xp_total),
            xp_store,

            sidebar_tab: SidebarTab::Files,
            sidebar_collapsed: false,
            quick_open: None,
            quick_open_focus: None,
            file_cache: None,

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
            prev_executed: HashMap::new(),
            error_line: None,
            flow_visible: false,
            code_scroll: gpui::UniformListScrollHandle::new(),

            asm_visible: false,
            asm: None,
            asm_gen: 0,
            asm_loading: false,
            asm_scroll: gpui::UniformListScrollHandle::new(),

            debug: crate::debug::DebugSession::new(),
            debug_visible: false,
            debug_term_restore: None,
            breakpoints: crate::debug::Breakpoints::from_map(ui.breakpoints.clone()),

            benchmarks: ui.benchmarks.clone(),
            bench_naming: None,
            bench_focus: None,

            ui_save_gen: 0,

            output: Vec::new(),
            output_visible: ui.terminal_visible.unwrap_or(true),
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
                // Quick Open's cached file list is now stale (§5.7).
                self.file_cache = None;
            }
            AppEvent::LspReady { handle, sync_kind } => self.on_lsp_ready(handle, sync_kind),
            AppEvent::Lsp(ev) => self.on_lsp_event(ev),
            AppEvent::Completion {
                generation,
                items,
                anchor,
            } => self.on_completion(generation, items, anchor),
            AppEvent::Hover {
                generation,
                text,
                row,
                col,
            } => self.on_hover(generation, text, row, col),
            AppEvent::Definition { path, line } => {
                self.open_file(path);
                self.reveal_line(line);
            }
            AppEvent::Ghost {
                generation,
                content,
                prefix,
                suffix,
                line_suffix,
                anchor,
                max_lines,
            } => self.on_ghost(generation, content, prefix, suffix, line_suffix, anchor, max_lines),
            AppEvent::AsmReady { generation, result } => self.on_asm_ready(generation, result),
            AppEvent::VarChildren { path, children } => {
                self.debug.set_children(path, children);
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
    /// Timer/gauge button: opens/closes the whole right sidebar (runtime
    /// graphs + training + telemetry). While it's open the bottom strip is
    /// fully hidden (render gate) — `output_visible` is left untouched so the
    /// terminal comes back exactly as it was when the sidebar closes.
    pub fn action_toggle_runtime(&mut self) {
        self.runtime_visible = !self.runtime_visible;
    }

    /// Toggle the execution-flow decorations (Flow chip / ⌘E, §4.8).
    pub fn action_toggle_flow(&mut self) {
        self.flow_visible = !self.flow_visible;
    }

    /// Cmd+Click glyph navigation (§4.8): reveal a target line in the viewer.
    /// The flow analysis is single-file, so all targets are in the active tab.
    pub fn flow_goto(&mut self, line_1based: usize) {
        if line_1based >= 1 {
            self.code_scroll
                .scroll_to_item(line_1based - 1, gpui::ScrollStrategy::Center);
        }
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
            // Count only real errors — clang's warnings ride along in `errors`
            // with their own severity, and leading with them buries the failure.
            let nerr = res
                .errors
                .iter()
                .filter(|e| e.severity == forge_build::Severity::Error)
                .count();
            self.status_line(&format!("[forge] Build failed ({nerr} error(s), {ms}ms)"));
            for e in &res.errors {
                let tag = match e.severity {
                    forge_build::Severity::Error => "error",
                    forge_build::Severity::Warning => "warning",
                    forge_build::Severity::Note => "note",
                };
                push_output(
                    &mut self.output,
                    &format!(
                        "  {}:{}:{}: {tag}: {}",
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
        // Snapshot rotation for the exec-annotation diff arrows: the run that was
        // `last_executed` becomes `prev_executed`, then this run becomes the new
        // `last_executed` (memory-decorations.ts:148-152).
        self.prev_executed = std::mem::take(&mut self.last_executed);
        self.last_executed = res.executed_lines.clone();
        // Error line (app.ts:1167-1172): on a failing run, the first
        // `path:line:col` in the sanitizer output marks the red error line.
        self.error_line = None;
        if res.exit_code != 0 {
            if let Some(san) = &res.sanitizer_output {
                self.error_line = parse_error_line(san);
            }
        }
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

    /// Populate the structural debug panel from LLDB events (§5.8): CONSOLE
    /// (ANSI-stripped), the FRAMES list + VARIABLES tree on a stop, and the exit
    /// status. Program output also flows to the console column.
    fn on_debug_event(&mut self, ev: DebugEvent) {
        match ev {
            DebugEvent::Output(s) => self.debug.push_console(&s),
            DebugEvent::Stopped {
                reason,
                file,
                line,
                frames,
                locals,
            } => {
                let refetch =
                    self.debug.on_stopped(reason.clone(), file.clone(), line, frames, locals);
                self.status_line(&format!("[forge] paused at {file}:{line} ({reason})"));
                self.reveal_line(line as usize);
                // Re-fetch children of paths that were expanded before the step so
                // the tree keeps its shape (values changed → cache was invalidated).
                for path in refetch {
                    self.request_var_children(&path);
                }
            }
            DebugEvent::Exited(code) => {
                self.debugging = false;
                self.debug.on_exited(code);
                self.status_line(&format!("[forge] debug exited ({code})"));
            }
        }
    }

    /// Show the debug panel, docking it above the terminal and hiding the bottom
    /// strip, remembering its prior visibility to restore on hide (§5.8).
    fn show_debug(&mut self) {
        if !self.debug_visible {
            self.debug_term_restore = Some(self.output_visible);
            self.debug_visible = true;
        }
    }

    /// Hide the debug panel, restoring the terminal's prior visibility (§5.8).
    pub fn hide_debug(&mut self) {
        self.debug_visible = false;
        if let Some(v) = self.debug_term_restore.take() {
            self.output_visible = v;
        }
    }

    // ── Debug session controls (§5.8 header buttons + F-keys) ─────────────────

    /// Continue / step over / into / out. Each is a no-op without a live driver.
    pub fn debug_continue(&mut self) {
        self.debug.status = crate::debug::DebugStatus::Running;
        self.run_driver(|d| Box::pin(async move { d.lock().await.continue_().await }));
    }
    pub fn debug_step_over(&mut self) {
        self.run_driver(|d| Box::pin(async move { d.lock().await.step_over().await }));
    }
    pub fn debug_step_into(&mut self) {
        self.run_driver(|d| Box::pin(async move { d.lock().await.step_into().await }));
    }
    pub fn debug_step_out(&mut self) {
        self.run_driver(|d| Box::pin(async move { d.lock().await.step_out().await }));
    }

    /// Select a stack frame (click navigates + reveals its line, §5.8).
    pub fn debug_select_frame(&mut self, index: usize) {
        self.debug.select_frame(index);
        if let Some(f) = self.debug.frames.get(index) {
            self.reveal_line(f.line as usize);
        }
    }

    /// Toggle a variable's expansion; lazily fetch its children on expand.
    pub fn debug_toggle_var(&mut self, path: &str) {
        if let Some(p) = self.debug.toggle_var(path) {
            self.request_var_children(&p);
        }
    }

    /// Spawn `get_var_children(path)` on the driver, forwarding the result back
    /// onto the pump as [`AppEvent::VarChildren`].
    fn request_var_children(&self, path: &str) {
        let Some(driver) = self.driver.clone() else {
            return;
        };
        let tx = self.app_tx.clone();
        let path = path.to_string();
        self.runtime.spawn(async move {
            let children = driver.lock().await.get_var_children(&path).await;
            let _ = tx.send(AppEvent::VarChildren { path, children });
        });
    }

    /// Run a `&LldbDriver` async op behind the shared lock (continue/step). The
    /// closure receives an owned `Arc` guard target; skipped when no session.
    fn run_driver<F>(&self, f: F)
    where
        F: FnOnce(
                std::sync::Arc<AsyncMutex<LldbDriver>>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            + Send
            + 'static,
    {
        let Some(driver) = self.driver.clone() else {
            return;
        };
        self.runtime.spawn(f(driver));
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
        self.error_line = None; // clear any prior run's error line
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
        self.status_line("[forge] Debug build (forced -O0)...");
        // Show the debug panel (docks above the terminal, §5.8) and mark running.
        self.show_debug();
        self.debug.on_running();

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
        // Breakpoints passed to the session at launch (§4.6).
        let breakpoints = self.breakpoints.all();
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
                if let Err(e) = d
                    .start(&exe.display().to_string(), &cwd, &breakpoints, &env)
                    .await
                {
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
        self.debug.status = crate::debug::DebugStatus::Idle;
        self.hide_debug();
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

    /// Swap forge-dark / forge-light (deliverable §3). Also swaps the editor's
    /// token palette so syntax highlighting re-resolves for the new theme (§4.2).
    pub fn action_theme(&mut self) {
        let light = !self.theme.is_light;
        self.theme = if light {
            Theme::forge_light()
        } else {
            Theme::forge_dark()
        };
        self.editor.set_palette(if light {
            TokenPalette::forge_light()
        } else {
            TokenPalette::forge_dark()
        });
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
            Ok(_) => {
                self.active_file = self.editor.active_path();
                self.dismiss_popups();
                self.pending_editor_focus = true; // caret + keys live immediately
                self.ensure_lsp();
                self.lsp_did_open(&path);
            }
            Err(e) => self.status_line(&format!("[forge] Could not open {}: {e}", path.display())),
        }
    }

    /// Open Folder (inventory §2, `app.ts:768` openWorkspace): show GPUI's native
    /// directory picker, then [`open_project`](Self::open_project) the chosen dir.
    /// The picker returns a oneshot receiver handled on the GPUI side (via
    /// `cx.spawn`, NOT tokio) so the resulting state update runs on the UI thread.
    /// Kept thin — all the real work is in the unit-testable `open_project`.
    pub fn prompt_open_project(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let dir = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None, // cancelled / error / empty
            };
            if let Some(dir) = dir {
                let _ = this.update(cx, |app, cx| {
                    app.open_project(dir);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Open `dir` as the workspace (inventory §2, `app.ts:812` doOpenWorkspace):
    /// repoint the tree + build/run target, restore that folder's persisted UI
    /// state (open tabs / breakpoints / benchmarks, same as
    /// startup), open the restored/first file through the real `open_file` path,
    /// and restart the fs-watch on the new root. `cx`-free so the state transition
    /// is unit-testable; the dialog handler drives `cx.notify` around it.
    pub fn open_project(&mut self, dir: PathBuf) {
        // Leaving any prior workspace: drop its clangd session so the new root
        // re-initializes lazily on the first file open (clangd is per-workspace).
        self.lsp = None;
        self.lsp_init_started = false;
        // Close the old tabs — a fresh workspace starts from its own tab set.
        while !self.editor.tabs.is_empty() {
            self.editor.close(0);
        }

        self.workspace_root = dir.clone();
        self.workspace_opened = true;
        self.tree = Some(FileTree::scan(dir.clone()));
        self.file_cache = None; // Quick Open cache is per-workspace (§5.7)

        // Restore this folder's persisted `ui` blob (§1.2), same fields startup
        // reads: breakpoints / benchmarks / ai toggle / terminal visibility.
        let ui = crate::workspace_state::load(&dir);
        self.breakpoints = crate::debug::Breakpoints::from_map(ui.breakpoints.clone());
        self.benchmarks = ui.benchmarks.clone();
        if let Some(v) = ui.ai_completion_enabled {
            self.ai_completion_enabled = v;
        }
        self.output_visible = ui.terminal_visible.unwrap_or(true);

        // Restore persisted open tabs (skipping deleted files), then the active
        // index — else fall back to the folder's first source file so the editor
        // isn't blank (mirrors --project's first-file pick).
        for tab in &ui.open_tabs {
            let p = PathBuf::from(&tab.path);
            if p.is_file() {
                let _ = self.editor.open(&p);
            }
        }
        if let Some(idx) = ui.active_tab_index {
            if idx >= 0 && (idx as usize) < self.editor.tabs.len() {
                self.editor.switch(idx as usize);
            }
        }
        if self.editor.active.is_none() {
            if let Some(first) = first_source_file(&dir) {
                let _ = self.editor.open(&first);
            }
        }
        self.active_file = self.editor.active_path();

        // Route the front tab through the real open path (LSP init + didOpen +
        // highlight + editor focus). No-op when the folder had no source file.
        if let Some(path) = self.active_file.clone() {
            self.open_file(path);
        }

        // Restart the fs-watch on the new root (§5.1).
        self.restart_fs_watch();

        // Status line reflects the opened folder (the build/run target follows
        // `active_file`, so both work against the new workspace immediately).
        self.status_line(&format!("[forge] Opened {}", dir.display()));
    }

    /// Drop the old fs-watch and start a fresh one on the current root. Dropping
    /// first releases the OS handle before the new watch registers; the debounce
    /// semantics in `workspace_tree` are unchanged.
    fn restart_fs_watch(&mut self) {
        self.fs_watcher = None;
        self.fs_watcher = (self.fs_watch)(&self.workspace_root);
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
        // did_close before removing (need the path). Silent close even if dirty —
        // matches the Electron app, which had no save prompt.
        if let Some(tab) = self.editor.tabs.get(index) {
            let path = tab.path.clone();
            if tab.lsp_opened {
                if let Some(lsp) = &self.lsp {
                    let _ = lsp.did_close(&path);
                }
            }
        }
        self.editor.close(index);
        self.active_file = self.editor.active_path();
        self.dismiss_popups();
    }

    // ── Structure panel + Quick Open (Phase-4 wave 3) ─────────────────────────

    /// Switch the left sidebar between FILES and STRUCTURE (§5.5).
    pub fn set_sidebar_tab(&mut self, tab: SidebarTab) {
        self.sidebar_tab = tab;
    }

    /// Collapse/expand the left sidebar (§2; app.ts:449-459): 260px ⇄ 28px strip.
    pub fn toggle_sidebar(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
    }

    /// The active tab's tree-sitter outline (empty when no tab / non-C-family).
    pub fn active_symbols(&self) -> &[Symbol] {
        self.editor
            .active_tab()
            .map(|t| t.symbols.as_slice())
            .unwrap_or(&[])
    }

    /// Reveal a 1-based source line in the code viewer by scrolling it to center
    /// (STRUCTURE click-to-navigate, §5.5). Same mechanism as `flow_goto`.
    pub fn reveal_line(&mut self, line: usize) {
        self.flow_goto(line);
    }

    /// Toggle the ⌘P Quick Open overlay (§5.7). Opening builds the file cache
    /// lazily (per workspace); closing just drops the transient state.
    pub fn toggle_quick_open(&mut self) {
        if self.quick_open.is_some() {
            self.quick_open = None;
        } else {
            self.ensure_file_cache();
            self.quick_open = Some(QuickOpenState::default());
        }
    }

    /// Close the Quick Open overlay.
    pub fn close_quick_open(&mut self) {
        self.quick_open = None;
    }

    /// Build the Quick Open file cache from a full workspace scan if absent
    /// (§5.7). Reuses the `workspace_tree` scan + ignore rules.
    fn ensure_file_cache(&mut self) {
        if self.file_cache.is_none() {
            let tree = FileTree::scan_full(self.workspace_root.clone());
            self.file_cache = Some(quick_open::flatten(&tree));
        }
    }

    /// The current Quick Open matches (filtered by the query, capped at 10). Empty
    /// when the overlay is closed.
    pub fn quick_open_matches(&self) -> Vec<Match> {
        let Some(state) = &self.quick_open else {
            return Vec::new();
        };
        let files = self.file_cache.as_deref().unwrap_or(&[]);
        quick_open::filter(files, &state.query, &self.workspace_root)
    }

    /// Apply one captured keystroke to the Quick Open overlay (§5.7): printable
    /// chars append, Backspace pops, ↑/↓ move, Enter opens, Esc closes.
    pub fn quick_open_key(&mut self, key: &str, key_char: Option<String>, printable: bool) {
        let matches = self.quick_open_matches();
        let action = match self.quick_open.as_mut() {
            Some(state) => state.on_key(key, key_char.as_deref(), printable, &matches),
            None => return,
        };
        match action {
            KeyAction::None => {}
            KeyAction::Close => self.close_quick_open(),
            KeyAction::Open(path) => self.quick_open_open(path),
        }
    }

    /// Open a file from Quick Open and close the overlay.
    pub fn quick_open_open(&mut self, path: PathBuf) {
        self.close_quick_open();
        self.open_file(path);
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

/// Char advance of Menlo at the editor font size (mouse↔column mapping). See the
/// note on `panels::code_view::CHAR_W`.
use crate::panels::code_view::LINE_H;

/// The first `.cpp`/`.cc`/`.mm` in `dir` alphabetically (inventory §2/§5): the
/// fallback tab `open_project` shows when a folder has no persisted open tabs,
/// mirroring `resolve_active_file`'s `--project` first-file pick.
fn first_source_file(dir: &Path) -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = std::fs::read_dir(dir)
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
    cands.into_iter().next()
}

/// clangd handles the C/C++ families; `.metal` is Metal (never clangd) and other
/// extensions are plain text. This is the §4.4 gating with the inventory's
/// c-family fix (the old app registered cpp only).
fn lsp_eligible(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some(
            "c" | "cc" | "cpp" | "cxx" | "c++" | "h" | "hpp" | "hxx" | "hh" | "inl" | "m" | "mm"
        )
    )
}

/// Files that get AI ghost text (§4.11 `LANGUAGES`): cpp/c/metal/objective-c plus
/// python/js/ts/shell. Broader than [`lsp_eligible`] (which is clangd-only).
fn ghost_eligible(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    matches!(
        ext.as_deref(),
        Some(
            "c" | "cc" | "cpp" | "cxx" | "c++" | "h" | "hpp" | "hxx" | "hh" | "inl" | "m" | "mm"
                | "metal" | "py" | "js" | "jsx" | "ts" | "tsx" | "sh" | "bash" | "zsh"
        )
    )
}

// ── Editable-editor + LSP behavior (E2) ───────────────────────────────────────
impl JadeApp {
    /// Monotonic milliseconds since app construction (decoration debounces).
    pub fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// The measured mono char advance (px) for caret/click column math.
    pub fn char_w(&self) -> f32 {
        f32::from_bits(self.editor_char_w.load(Ordering::Relaxed)).max(1.0)
    }

    /// Close the autocomplete + hover popups and dismiss any ghost text.
    pub fn dismiss_popups(&mut self) {
        self.completion = None;
        self.hover = None;
        self.hover_target = None;
        self.ghost = None;
    }

    /// Dispatch one editor keystroke; returns true when consumed (the caller then
    /// stops propagation so the key doesn't bubble / reach the IME). Character
    /// keys return false so macOS routes them to `replace_text_in_range`.
    pub fn editor_key(&mut self, ks: &gpui::Keystroke, cx: &mut Context<Self>) -> bool {
        let m = ks.modifiers;
        let key = ks.key.as_str();
        let shift = m.shift;

        // Ghost text gates BEFORE the completion popup (seam): a plain Tab accepts
        // the ghost, Esc dismisses it. Only when no chord modifiers are held.
        if self.ghost.is_some() && !m.platform && !m.control && !m.alt {
            match key {
                "tab" => {
                    self.ghost_accept(cx);
                    return true;
                }
                "escape" => {
                    self.ghost = None;
                    return true;
                }
                _ => {}
            }
        }

        // Popup-first interception.
        if self.completion.is_some() {
            match key {
                "up" => {
                    self.completion_move(-1);
                    return true;
                }
                "down" => {
                    self.completion_move(1);
                    return true;
                }
                "enter" | "tab" => {
                    self.completion_accept(cx);
                    return true;
                }
                "escape" => {
                    self.completion = None;
                    return true;
                }
                _ => {}
            }
        }
        if self.hover.is_some() && key == "escape" {
            self.hover = None;
            return true;
        }

        // ⌘ chords.
        if m.platform && !m.control && !m.alt {
            match key {
                "s" => self.editor_save(),
                "w" => {
                    if let Some(i) = self.editor.active {
                        self.close_tab(i);
                    }
                }
                "a" => {
                    // ⌘⇧A toggles the ASM viewer (§3); ⌘A selects all.
                    if shift {
                        self.toggle_asm(cx);
                    } else {
                        self.buf_move(|b, _| b.select_all(), false);
                    }
                }
                "z" => {
                    if shift {
                        self.editor_redo(cx);
                    } else {
                        self.editor_undo(cx);
                    }
                }
                "c" => self.editor_copy(cx),
                "x" => self.editor_cut(cx),
                "v" => self.editor_paste(cx),
                "left" => self.buf_move(|b, e| b.move_home(e), shift),
                "right" => self.buf_move(|b, e| b.move_end(e), shift),
                "up" => self.buf_move(|b, e| b.move_doc_start(e), shift),
                "down" => self.buf_move(|b, e| b.move_doc_end(e), shift),
                _ => return false, // let ⌘P etc. bubble
            }
            return true;
        }

        // ⌥ chords (word nav + word delete).
        if m.alt && !m.platform {
            match key {
                "left" => self.buf_move(|b, e| b.move_word_left(e), shift),
                "right" => self.buf_move(|b, e| b.move_word_right(e), shift),
                "backspace" => {
                    let r = self.with_edit(|b| b.delete_word_back());
                    self.after_edit(r, cx);
                }
                _ => return false,
            }
            return true;
        }

        // Plain named keys.
        match key {
            "left" => self.buf_move(|b, e| b.move_left(e), shift),
            "right" => self.buf_move(|b, e| b.move_right(e), shift),
            "up" => self.buf_move(|b, e| b.move_up(e), shift),
            "down" => self.buf_move(|b, e| b.move_down(e), shift),
            "home" => self.buf_move(|b, e| b.move_home(e), shift),
            "end" => self.buf_move(|b, e| b.move_end(e), shift),
            "pageup" => self.editor_page(-1, shift),
            "pagedown" => self.editor_page(1, shift),
            "backspace" => {
                let r = self.with_edit(|b| b.delete_backward());
                self.after_edit(r, cx);
            }
            "delete" => {
                let r = self.with_edit(|b| b.delete_forward());
                self.after_edit(r, cx);
            }
            "enter" => {
                let r = self.with_edit(|b| b.insert_newline());
                self.after_edit(r, cx);
            }
            "tab" => {
                let r = self.with_edit(|b| b.insert_tab());
                self.after_edit(r, cx);
            }
            "escape" if self.completion.is_some() || self.hover.is_some() => {
                self.dismiss_popups()
            }
            _ => return false, // escape-with-nothing / character key → IME
        }
        true
    }

    /// Apply a cursor-only motion to the active buffer, dismiss popups, follow.
    /// Any caret movement or edit: hold the caret solid for the next blink
    /// window so it never flickers while typing/navigating.
    fn caret_activity(&mut self) {
        self.caret_last_active = self.now_ms();
        self.caret_blink_show = true;
    }

    fn buf_move(&mut self, f: impl FnOnce(&mut forge_buffer::Buffer, bool), extend: bool) {
        if let Some(tab) = self.editor.active_tab_mut() {
            f(&mut tab.buffer, extend);
        }
        self.caret_activity();
        self.completion = None;
        self.hover = None;
        self.hover_target = None;
        self.ghost = None; // any caret move dismisses ghost text (§4.11)
        self.scroll_caret_into_view();
        self.sync_asm_selection(); // keep the ASM cross-highlight aligned (§6)
    }

    /// Page up/down by the current viewport height in rows.
    fn editor_page(&mut self, dir: i32, extend: bool) {
        let rows = (self.editor_rows.load(Ordering::Relaxed) as usize).max(1);
        if let Some(tab) = self.editor.active_tab_mut() {
            for _ in 0..rows {
                if dir < 0 {
                    tab.buffer.move_up(extend);
                } else {
                    tab.buffer.move_down(extend);
                }
            }
        }
        self.completion = None;
        self.ghost = None;
        self.scroll_caret_into_view();
    }

    /// Run a text-changing buffer op on the active tab, returning its record.
    fn with_edit(
        &mut self,
        f: impl FnOnce(&mut forge_buffer::Buffer) -> forge_buffer::EditRecord,
    ) -> forge_buffer::EditRecord {
        match self.editor.active_tab_mut() {
            Some(tab) => f(&mut tab.buffer),
            None => forge_buffer::EditRecord {
                changes: Vec::new(),
                version: 0,
            },
        }
    }

    /// Shared post-edit bookkeeping: recompute eager decorations + arm debounces,
    /// forward an incremental `didChange`, follow the caret, wake the debounce.
    fn after_edit(&mut self, record: forge_buffer::EditRecord, cx: &mut Context<Self>) {
        self.caret_activity();
        let now = self.now_ms();
        let payload = self.editor.active_tab_mut().map(|tab| {
            tab.on_edited(now);
            (
                tab.path.clone(),
                tab.lsp_opened,
                tab.lsp_version(),
                tab.buffer.to_string(),
                editor_view::record_to_lsp_edits(&record),
            )
        });
        if let Some((path, opened, version, text, edits)) = payload {
            if !record.is_noop() && opened {
                self.lsp_did_change(&path, edits, text, version);
            }
        }
        // XP credit for newline-completing edits (§4.10), then (re)request ghost
        // text at the new caret (§4.11). Both hang off this single choke point.
        self.credit_xp(&record, now);
        self.scroll_caret_into_view();
        self.schedule_ghost(cx);
        self.ensure_decoration_wake(cx);
        // ASM viewer (§6): keep the cross-highlight aligned with the caret and
        // auto-refresh the listing 1.5s after the edit settles while visible.
        self.sync_asm_selection();
        self.schedule_asm_refresh(cx);
    }

    /// Award XP for edits whose change inserted a newline and whose completed line
    /// (comments stripped) ends with `;`, ≤300 chars (§4.10). The streak scales
    /// the award and persists globally.
    fn credit_xp(&mut self, record: &forge_buffer::EditRecord, now_ms: u64) {
        let mut credits = 0u64;
        if let Some(tab) = self.editor.active_tab() {
            for ch in &record.changes {
                if !ch.new_text.contains('\n') || ch.new_text.chars().count() > 300 {
                    continue;
                }
                let row = ch.start.line;
                if row >= tab.line_count() {
                    continue;
                }
                let line = tab.buffer.line(row);
                if crate::xp::edit_earns_credit(&ch.new_text, &line) {
                    credits += 1;
                }
            }
        }
        if credits > 0 {
            self.xp.credit(credits, now_ms);
            self.xp_store.save(self.xp.total());
        }
    }

    /// The topmost visible editor row (from the uniform-list scroll handle).
    pub fn editor_scroll_top(&self) -> usize {
        // NOT base_handle.top_item(): that walks `child_bounds`, which a
        // virtualized uniform_list never populates, so it always returned 0.
        // With the viewport scrolled, every click then looked "out of view"
        // to scroll_caret_into_view and snapped the list — the "editor jumps
        // around when I click" bug. Rows are fixed LINE_H by construction, so
        // derive the first FULLY visible row from the scroll offset (the
        // handle's last_item_size.item is the viewport, not a row — measured).
        let scrolled = -f32::from(self.code_scroll.0.borrow().base_handle.offset().y);
        (((scrolled - crate::panels::code_view::PAD_TOP) / LINE_H).ceil().max(0.0)) as usize
    }

    /// Minimal follow-scroll: bring the caret row into the visible window only
    /// when it's currently outside it (no jarring recentre on every keystroke).
    fn scroll_caret_into_view(&mut self) {
        let Some(tab) = self.editor.active_tab() else {
            return;
        };
        let row = tab.caret_point().row;
        let top = self.editor_scroll_top();
        let rows = (self.editor_rows.load(Ordering::Relaxed) as usize).max(1);
        if row < top {
            self.code_scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Top);
        } else if row >= top + rows {
            self.code_scroll
                .scroll_to_item(row + 1 - rows, gpui::ScrollStrategy::Top);
        }
    }

    /// Spawn the decoration-recompute wake loop if one isn't already running and
    /// some tab has a pending debounce.
    fn ensure_decoration_wake(&mut self, cx: &mut Context<Self>) {
        if self.decoration_wake_running
            || !self.editor.tabs.iter().any(|t| t.decorations_pending())
        {
            return;
        }
        self.decoration_wake_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let still = this.update(cx, |app, cx| {
                    let now = app.now_ms();
                    let mut changed = false;
                    for tab in &mut app.editor.tabs {
                        changed |= tab.poll_decorations(now);
                    }
                    if changed {
                        cx.notify();
                    }
                    app.editor.tabs.iter().any(|t| t.decorations_pending())
                });
                match still {
                    Ok(true) => continue,
                    _ => break,
                }
            }
            let _ = this.update(cx, |app, _| app.decoration_wake_running = false);
        })
        .detach();
    }

    // ── Save / undo / redo / clipboard ────────────────────────────────────────

    /// ⌘S: write the buffer to disk, mark saved, notify clangd.
    pub fn editor_save(&mut self) {
        let Some((path, text)) = self
            .editor
            .active_tab()
            .map(|t| (t.path.clone(), t.buffer.to_string()))
        else {
            return;
        };
        match std::fs::write(&path, text.as_bytes()) {
            Ok(()) => {
                if let Some(tab) = self.editor.tab_mut_for(&path) {
                    tab.buffer.mark_saved();
                }
                self.lsp_did_save(&path);
                self.status_line(&format!("[forge] Saved {}", path.display()));
            }
            Err(e) => self.status_line(&format!("[forge] Save failed: {e}")),
        }
    }

    fn editor_undo(&mut self, cx: &mut Context<Self>) {
        self.editor_history(true, cx);
    }

    fn editor_redo(&mut self, cx: &mut Context<Self>) {
        self.editor_history(false, cx);
    }

    /// Undo/redo change text without an incremental record, so we resync clangd
    /// with a full-text `didChange`.
    fn editor_history(&mut self, undo: bool, cx: &mut Context<Self>) {
        let now = self.now_ms();
        let payload = self.editor.active_tab_mut().and_then(|tab| {
            let changed = if undo { tab.buffer.undo() } else { tab.buffer.redo() };
            if !changed {
                return None;
            }
            tab.on_edited(now);
            Some((
                tab.path.clone(),
                tab.lsp_opened,
                tab.lsp_version(),
                tab.buffer.to_string(),
            ))
        });
        if let Some((path, opened, version, text)) = payload {
            if opened {
                if let Some(lsp) = &self.lsp {
                    let _ = lsp.did_change(&path, DidChange::Full(text), version);
                }
            }
        }
        self.dismiss_popups();
        self.scroll_caret_into_view();
        self.ensure_decoration_wake(cx);
    }

    /// The selected text of the active buffer, if any.
    fn selected_text(&self) -> Option<String> {
        let tab = self.editor.active_tab()?;
        let sel = tab.buffer.selection();
        if sel.is_empty() {
            return None;
        }
        Some(tab.buffer.to_string()[sel.range()].to_string())
    }

    fn editor_copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn editor_cut(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            let r = self.with_edit(|b| b.delete_backward());
            self.after_edit(r, cx);
        }
    }

    fn editor_paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) else {
            return;
        };
        let r = self.with_edit(|b| b.insert_text(&text));
        self.after_edit(r, cx);
    }

    // ── Mouse ─────────────────────────────────────────────────────────────────

    /// Left mouse-down on row `row` at window x `x` (click_count drives the
    /// caret/word/line selection).
    pub fn editor_mouse_down(
        &mut self,
        row: usize,
        x: f32,
        shift: bool,
        clicks: usize,
    ) {
        self.dismiss_popups();
        let text_left = f32::from_bits(self.editor_text_left.load(Ordering::Relaxed));
        let cw = self.char_w();
        let selecting = {
            let Some(tab) = self.editor.active_tab_mut() else {
                return;
            };
            let row = row.min(tab.line_count().saturating_sub(1));
            let line = tab.buffer.line(row).into_owned();
            let maxcol = line.chars().count();
            let col = editor_view::px_to_col(x - text_left, cw, maxcol);
            let byte = tab.buffer.point_to_offset(Point::new(row, col));
            match clicks {
                n if n >= 3 => {
                    let start = tab.buffer.point_to_offset(Point::new(row, 0));
                    let end = if row + 1 < tab.line_count() {
                        tab.buffer.point_to_offset(Point::new(row + 1, 0))
                    } else {
                        tab.buffer.len_bytes()
                    };
                    tab.buffer.set_selection(Selection::new(start, end));
                    false
                }
                2 => {
                    let wr = editor_view::word_range_in_line(&line, col);
                    let s = tab.buffer.point_to_offset(Point::new(row, wr.start));
                    let e = tab.buffer.point_to_offset(Point::new(row, wr.end));
                    tab.buffer.set_selection(Selection::new(s, e));
                    false
                }
                _ => {
                    if shift {
                        let anchor = tab.buffer.selection().anchor;
                        tab.buffer.set_selection(Selection::new(anchor, byte));
                    } else {
                        tab.buffer.set_caret(byte);
                    }
                    true
                }
            }
        };
        self.editor_selecting = selecting;
        self.caret_activity();
        self.sync_asm_selection(); // source caret → asm cross-highlight (§6)
    }

    /// Left-drag: extend the selection head to the pointer.
    pub fn editor_mouse_drag(&mut self, row: usize, x: f32) {
        if !self.editor_selecting {
            return;
        }
        let text_left = f32::from_bits(self.editor_text_left.load(Ordering::Relaxed));
        let cw = self.char_w();
        {
            let Some(tab) = self.editor.active_tab_mut() else {
                return;
            };
            let row = row.min(tab.line_count().saturating_sub(1));
            let maxcol = tab.buffer.line(row).chars().count();
            let col = editor_view::px_to_col(x - text_left, cw, maxcol);
            let byte = tab.buffer.point_to_offset(Point::new(row, col));
            let anchor = tab.buffer.selection().anchor;
            tab.buffer.set_selection(Selection::new(anchor, byte));
        }
        self.caret_activity();
        self.scroll_caret_into_view();
    }

    /// End a drag-select.
    pub fn editor_mouse_up(&mut self) {
        self.editor_selecting = false;
    }

    /// Gutter line-number click: caret to the start of row `row` (shift extends
    /// the selection there, like a plain in-text click at column 0).
    pub fn editor_caret_to_line_start(&mut self, row: usize, shift: bool) {
        self.dismiss_popups();
        let Some(tab) = self.editor.active_tab_mut() else {
            return;
        };
        let row = row.min(tab.line_count().saturating_sub(1));
        let byte = tab.buffer.point_to_offset(Point::new(row, 0));
        if shift {
            let anchor = tab.buffer.selection().anchor;
            tab.buffer.set_selection(Selection::new(anchor, byte));
        } else {
            tab.buffer.set_caret(byte);
        }
        self.caret_activity();
        self.sync_asm_selection();
    }

    // ── Completion ────────────────────────────────────────────────────────────

    /// Called after IME text insertion: (re)filter an open popup and, on an
    /// identifier/trigger char, (re)request completion; otherwise dismiss.
    fn on_text_inserted(&mut self, inserted: &str, cx: &mut Context<Self>) {
        let last = inserted.chars().last();
        let is_ident = last.is_some_and(|c| c.is_alphanumeric() || c == '_');
        let is_trigger = last.is_some_and(|c| matches!(c, '.' | ':' | '>' | '<' | '"' | '/'));
        if !is_ident && !is_trigger {
            self.completion = None;
            return;
        }
        self.refresh_completion_filter();
        self.schedule_completion(cx);
        // Frequency completion (§4.12) is local + instant — merge it now so the
        // popup shows even before (or without) an LSP response.
        self.refresh_frequency_completion();
    }

    /// Frequency-completion items for the identifier being typed at the caret
    /// (§4.12). Empty when there is no in-progress word or no eligible tab.
    fn frequency_items_for_caret(&self) -> Vec<CompletionItem> {
        let Some(tab) = self.editor.active_tab() else {
            return Vec::new();
        };
        if !tab.is_code() {
            return Vec::new();
        }
        let caret = tab.buffer.selection().caret();
        let ident = editor_view::ident_range_before(&tab.buffer, caret);
        let text = tab.buffer.to_string();
        let current_word = text[ident].to_string();
        // Mirror the TS `getWordUntilPosition` gate: only suggest mid-word.
        if current_word.is_empty() {
            return Vec::new();
        }
        crate::frequency::completion_items(&text, &current_word)
    }

    /// Merge frequency suggestions into the popup (creating it if the LSP hasn't
    /// answered yet). Dedupe by label — LSP items win (§4.12 merge rule).
    fn refresh_frequency_completion(&mut self) {
        let freq = self.frequency_items_for_caret();
        let (prefix, anchor) = match self.editor.active_tab() {
            Some(tab) => {
                let caret = tab.buffer.selection().caret();
                let ident = editor_view::ident_range_before(&tab.buffer, caret);
                let p = tab.buffer.to_string()[ident].to_string();
                let pt = tab.caret_point();
                (p, (pt.row, pt.col))
            }
            None => return,
        };
        match &mut self.completion {
            Some(c) => {
                for f in freq {
                    if !c.items.iter().any(|it| it.label == f.label) {
                        c.items.push(f);
                    }
                }
                c.filtered = editor_view::completion_filter(&c.items, &prefix);
                if c.filtered.is_empty() {
                    self.completion = None;
                } else {
                    c.selected = c.selected.min(c.filtered.len() - 1);
                }
            }
            None => {
                if freq.is_empty() {
                    return;
                }
                let filtered = editor_view::completion_filter(&freq, &prefix);
                if !filtered.is_empty() {
                    self.completion = Some(CompletionState {
                        items: freq,
                        filtered,
                        selected: 0,
                        anchor,
                    });
                }
            }
        }
    }

    /// Re-narrow an open popup against the currently-typed prefix.
    fn refresh_completion_filter(&mut self) {
        let prefix = match self.editor.active_tab() {
            Some(tab) => {
                let caret = tab.buffer.selection().caret();
                let ident = editor_view::ident_range_before(&tab.buffer, caret);
                tab.buffer.to_string()[ident].to_string()
            }
            None => return,
        };
        if let Some(c) = &mut self.completion {
            c.filtered = editor_view::completion_filter(&c.items, &prefix);
            c.selected = 0;
            if c.filtered.is_empty() {
                self.completion = None;
            }
        }
    }

    /// Request completion at the caret, debounced 80ms, superseding older
    /// requests via the generation counter.
    fn schedule_completion(&mut self, _cx: &mut Context<Self>) {
        let Some(lsp) = self.lsp.clone() else {
            return;
        };
        let Some(tab) = self.editor.active_tab() else {
            return;
        };
        if !lsp_eligible(&tab.path) {
            return;
        }
        let caret = tab.buffer.selection().caret();
        let pos = tab.buffer.offset_to_lsp(caret);
        let point = tab.caret_point();
        let path = tab.path.clone();
        self.completion_gen += 1;
        let generation = self.completion_gen;
        let tx = self.app_tx.clone();
        let anchor = (point.row, point.col);
        self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let position = forge_lsp::Position::new(pos.line as u32, pos.character as u32);
            if let Ok(items) = lsp.completion(&path, position).await {
                let _ = tx.send(AppEvent::Completion {
                    generation,
                    items,
                    anchor,
                });
            }
        });
    }

    fn on_completion(&mut self, generation: u64, items: Vec<CompletionItem>, anchor: (usize, usize)) {
        if generation != self.completion_gen {
            return; // superseded
        }
        let prefix = match self.editor.active_tab() {
            Some(tab) => {
                let caret = tab.buffer.selection().caret();
                let ident = editor_view::ident_range_before(&tab.buffer, caret);
                tab.buffer.to_string()[ident].to_string()
            }
            None => return,
        };
        // Merge frequency completion (§4.12) into the LSP results — dedupe by
        // label, LSP wins (LSP items keep their server order first, then the
        // frequency words ranked by count).
        let mut merged = items;
        for f in self.frequency_items_for_caret() {
            if !merged.iter().any(|it| it.label == f.label) {
                merged.push(f);
            }
        }
        if merged.is_empty() {
            self.completion = None;
            return;
        }
        let filtered = editor_view::completion_filter(&merged, &prefix);
        if filtered.is_empty() {
            self.completion = None;
            return;
        }
        self.completion = Some(CompletionState {
            items: merged,
            filtered,
            selected: 0,
            anchor,
        });
    }

    /// Move the popup selection (wraps within the filtered list).
    pub fn completion_move(&mut self, delta: i32) {
        if let Some(c) = &mut self.completion {
            let n = c.filtered.len() as i32;
            if n == 0 {
                return;
            }
            c.selected = (((c.selected as i32 + delta) % n + n) % n) as usize;
        }
    }

    /// Accept the highlighted completion, applying its textEdit / insertText.
    pub fn completion_accept(&mut self, cx: &mut Context<Self>) {
        let item = self.completion.as_ref().and_then(|c| c.current().cloned());
        self.completion = None;
        let Some(item) = item else {
            return;
        };
        let record = match self.editor.active_tab_mut() {
            Some(tab) => {
                let caret = tab.buffer.selection().caret();
                let ident = editor_view::ident_range_before(&tab.buffer, caret);
                let (range, text) = editor_view::completion_edit(&item, &tab.buffer, ident);
                Some(tab.buffer.edit(range, &text))
            }
            None => None,
        };
        if let Some(record) = record {
            self.after_edit(record, cx);
        }
    }

    // ── AI ghost text (§4.11) ─────────────────────────────────────────────────

    /// Toggle whether ghost text is offered (`aiCompletionEnabled`). Disabling
    /// immediately clears any visible suggestion.
    pub fn action_toggle_ai_completion(&mut self) {
        self.ai_completion_enabled = !self.ai_completion_enabled;
        if !self.ai_completion_enabled {
            self.ghost = None;
        }
    }

    /// Toggle multiline ghost mode (`aiMultiline`). Cached output was generated
    /// for the old mode, so the cache is cleared (§4.11).
    pub fn action_toggle_ai_multiline(&mut self) {
        self.ai_multiline = !self.ai_multiline;
        self.ghost_cache.clear();
        self.ghost = None;
    }

    /// (Re)compute ghost text at the caret (§4.11): serve a cache hit instantly
    /// (exact or typed-through), else debounce 120ms and request `/infill`
    /// (forge-ai's single-flight aborts any older request). Only fires when
    /// `aiCompletionEnabled` and the backend is `Ready` at a collapsed caret.
    fn schedule_ghost(&mut self, _cx: &mut Context<Self>) {
        if !self.ai_completion_enabled || self.ai_status.state != AiState::Ready {
            self.ghost = None;
            return;
        }
        let Some(tab) = self.editor.active_tab() else {
            self.ghost = None;
            return;
        };
        if !ghost_eligible(&tab.path) || !tab.buffer.selection().is_empty() {
            self.ghost = None;
            return;
        }
        let caret = tab.buffer.selection().caret();
        let full = tab.buffer.to_string();
        let prefix = crate::ghost::cap_prefix(&full[..caret], crate::ghost::MAX_PREFIX_CHARS);
        let suffix = crate::ghost::cap_suffix(&full[caret..], crate::ghost::MAX_SUFFIX_CHARS);
        let point = tab.caret_point();
        let line = tab.buffer.line(point.row).into_owned();
        let line_suffix: String = line.chars().skip(point.col).collect();
        let anchor = (point.row, point.col);
        let path = tab.path.clone();
        let max_lines = if self.ai_multiline { crate::ghost::MAX_LINES } else { 1 };

        // Cache hit → serve immediately, no request.
        if let Some(cached) = self.ghost_cache.lookup(&prefix, &suffix) {
            self.ghost = crate::ghost::post_process(&cached, &line_suffix, max_lines)
                .map(|text| GhostState { text, anchor });
            return;
        }

        // Cache miss: clear the stale suggestion and issue a debounced request.
        self.ghost = None;
        self.ghost_gen += 1;
        let generation = self.ghost_gen;
        let ai = self.ai.clone();
        let tx = self.app_tx.clone();
        let single_line = !self.ai_multiline;
        let filename = Some(path.display().to_string());
        self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(crate::ghost::DEBOUNCE_MS)).await;
            let result = ai
                .infill(&InfillRequest {
                    prefix: prefix.clone(),
                    suffix: suffix.clone(),
                    filename,
                    single_line,
                })
                .await;
            let content = result.map(|r| r.content);
            let _ = tx.send(AppEvent::Ghost {
                generation,
                content,
                prefix,
                suffix,
                line_suffix,
                anchor,
                max_lines,
            });
        });
    }

    /// Handle a `/infill` response: cache the raw output and post-process it into
    /// the ghost run (or suppress it). Stale generations are dropped.
    #[allow(clippy::too_many_arguments)]
    fn on_ghost(
        &mut self,
        generation: u64,
        content: Option<String>,
        prefix: String,
        suffix: String,
        line_suffix: String,
        anchor: (usize, usize),
        max_lines: usize,
    ) {
        if generation != self.ghost_gen {
            return; // superseded
        }
        let Some(raw) = content else {
            self.ghost = None;
            return;
        };
        if raw.is_empty() {
            self.ghost = None;
            return;
        }
        self.ghost_cache.put(prefix, suffix, raw.clone());
        self.ghost = crate::ghost::post_process(&raw, &line_suffix, max_lines)
            .map(|text| GhostState { text, anchor });
    }

    /// Accept the ghost text (Tab): insert it at the caret.
    fn ghost_accept(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.ghost.take() else {
            return;
        };
        let record = self.with_edit(|b| b.insert_text(&g.text));
        self.after_edit(record, cx);
    }

    // ── Hover ─────────────────────────────────────────────────────────────────

    /// Pointer moved over the code at row `row`, window x `x`: schedule a hover
    /// request after a 300ms dwell if the target cell changed.
    pub fn editor_hover_move(&mut self, row: usize, x: f32) {
        if self.editor_selecting {
            return;
        }
        let text_left = f32::from_bits(self.editor_text_left.load(Ordering::Relaxed));
        let cw = self.char_w();
        let (path, pos, cell) = {
            let Some(tab) = self.editor.active_tab() else {
                return;
            };
            if !lsp_eligible(&tab.path) {
                return;
            }
            let row = row.min(tab.line_count().saturating_sub(1));
            let maxcol = tab.buffer.line(row).chars().count();
            let col = editor_view::px_to_col(x - text_left, cw, maxcol);
            let byte = tab.buffer.point_to_offset(Point::new(row, col));
            (tab.path.clone(), tab.buffer.offset_to_lsp(byte), (row, col))
        };
        if self.hover_target == Some(cell) {
            return; // same cell — don't re-request
        }
        self.hover_target = Some(cell);
        self.hover = None;
        let Some(lsp) = self.lsp.clone() else {
            return;
        };
        self.hover_gen += 1;
        let generation = self.hover_gen;
        let tx = self.app_tx.clone();
        let (row, col) = cell;
        self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let position = forge_lsp::Position::new(pos.line as u32, pos.character as u32);
            let text = match lsp.hover(&path, position).await {
                Ok(Some(h)) => Some(flatten_hover(&h)),
                _ => None,
            };
            let _ = tx.send(AppEvent::Hover {
                generation,
                text,
                row,
                col,
            });
        });
    }

    fn on_hover(&mut self, generation: u64, text: Option<String>, row: usize, col: usize) {
        if generation != self.hover_gen {
            return;
        }
        match text {
            Some(t) if !t.trim().is_empty() => {
                self.hover = Some(HoverState { text: t, row, col })
            }
            _ => self.hover = None,
        }
    }

    /// ⌘-click: request the definition at row `row`, window x `x`, and open it.
    pub fn editor_goto_definition(&mut self, row: usize, x: f32) {
        let text_left = f32::from_bits(self.editor_text_left.load(Ordering::Relaxed));
        let cw = self.char_w();
        let (path, pos) = {
            let Some(tab) = self.editor.active_tab() else {
                return;
            };
            if !lsp_eligible(&tab.path) {
                return;
            }
            let row = row.min(tab.line_count().saturating_sub(1));
            let maxcol = tab.buffer.line(row).chars().count();
            let col = editor_view::px_to_col(x - text_left, cw, maxcol);
            let byte = tab.buffer.point_to_offset(Point::new(row, col));
            (tab.path.clone(), tab.buffer.offset_to_lsp(byte))
        };
        let Some(lsp) = self.lsp.clone() else {
            return;
        };
        let tx = self.app_tx.clone();
        self.runtime.spawn(async move {
            let position = forge_lsp::Position::new(pos.line as u32, pos.character as u32);
            if let Ok(locs) = lsp.definition(&path, position).await {
                if let Some(loc) = locs.into_iter().next() {
                    let target = path_from_uri(&loc.uri);
                    let _ = tx.send(AppEvent::Definition {
                        path: target,
                        line: loc.range.start.line as usize + 1,
                    });
                }
            }
        });
    }

    // ── LSP lifecycle ─────────────────────────────────────────────────────────

    /// Kick off `clangd` initialize once per workspace (spawned on the runtime).
    fn ensure_lsp(&mut self) {
        if self.lsp_init_started {
            return;
        }
        self.lsp_init_started = true;
        let root = self.workspace_root.clone();
        let include = self.lsp_include.clone();
        let tx = self.app_tx.clone();
        self.runtime.spawn(async move {
            match LspClient::initialize(&root, include.as_deref()).await {
                Ok(mut handle) => {
                    let sync_kind = handle.sync_kind();
                    if let Some(mut events) = handle.take_events() {
                        let etx = tx.clone();
                        tokio::spawn(async move {
                            while let Some(ev) = events.recv().await {
                                if etx.send(AppEvent::Lsp(ev)).is_err() {
                                    break;
                                }
                            }
                        });
                    }
                    let handle = Arc::new(handle);
                    let _ = tx.send(AppEvent::LspReady { handle, sync_kind });
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::BuildOutput(format!(
                        "[forge] clangd unavailable: {e}"
                    )));
                }
            }
        });
    }

    fn on_lsp_ready(&mut self, handle: Arc<LspHandle>, sync_kind: TextDocumentSyncKind) {
        self.lsp = Some(handle);
        self.lsp_sync_kind = sync_kind;
        // didOpen every already-open C-family tab.
        let paths: Vec<PathBuf> = self
            .editor
            .tabs
            .iter()
            .filter(|t| lsp_eligible(&t.path))
            .map(|t| t.path.clone())
            .collect();
        for p in paths {
            self.lsp_did_open(&p);
        }
    }

    fn on_lsp_event(&mut self, ev: LspEvent) {
        match ev {
            LspEvent::Ready => {}
            LspEvent::Diagnostics { path, diagnostics } => {
                if let Some(tab) = self.editor.tab_mut_for(&path) {
                    tab.diagnostics = diagnostics;
                }
            }
            LspEvent::Exited => {
                self.lsp = None;
                for tab in &mut self.editor.tabs {
                    tab.lsp_opened = false;
                }
                self.status_line("[forge] clangd exited");
            }
        }
    }

    /// Send `didOpen` for `path` if clangd is up, the file is eligible, and it
    /// hasn't been opened yet.
    fn lsp_did_open(&mut self, path: &Path) {
        let Some(lsp) = self.lsp.clone() else {
            return;
        };
        if !lsp_eligible(path) {
            return;
        }
        let Some(tab) = self.editor.tab_mut_for(path) else {
            return;
        };
        if tab.lsp_opened {
            return;
        }
        let text = tab.buffer.to_string();
        let version = tab.lsp_version();
        tab.lsp_opened = true;
        let _ = lsp.did_open(path, &text, version);
    }

    /// Forward an incremental (or full, when clangd negotiated full sync)
    /// `didChange`.
    fn lsp_did_change(
        &self,
        path: &Path,
        edits: Vec<forge_lsp::Utf16RangeEdit>,
        full_text: String,
        version: i32,
    ) {
        let Some(lsp) = &self.lsp else {
            return;
        };
        let change = if self.lsp_sync_kind == TextDocumentSyncKind::INCREMENTAL {
            DidChange::Incremental(edits)
        } else {
            DidChange::Full(full_text)
        };
        let _ = lsp.did_change(path, change, version);
    }

    fn lsp_did_save(&self, path: &Path) {
        if let Some(lsp) = &self.lsp {
            let _ = lsp.did_save(path);
        }
    }

    /// Aggregate diagnostic counts across the active tab (action-bar badges).
    pub fn active_diag_counts(&self) -> (usize, usize, usize) {
        self.editor
            .active_tab()
            .map(|t| editor_view::diagnostic_counts(&t.diagnostics))
            .unwrap_or((0, 0, 0))
    }

    // ── ASM viewer (§6) ───────────────────────────────────────────────────────

    /// Toggle the right-half ASM overlay (ASM chip / ⌘⇧A). Opening kicks off a
    /// `generate_asm` for the active file (saving first, app.ts:399); closing just
    /// drops the overlay (the listing is kept so a re-open is instant).
    pub fn toggle_asm(&mut self, cx: &mut Context<Self>) {
        self.asm_visible = !self.asm_visible;
        if self.asm_visible {
            self.refresh_asm(cx);
            self.sync_asm_selection();
        }
    }

    /// (Re)generate the assembly for the active file (§6). Saves the buffer first
    /// (so the asm matches on-screen code), then spawns `generate_asm` on the
    /// engine, superseding any in-flight request via the generation counter.
    fn refresh_asm(&mut self, _cx: &mut Context<Self>) {
        let Some(file) = self.active_file.clone() else {
            return;
        };
        // Save first so the assembly reflects the current buffer (app.ts:399).
        self.editor_save();
        self.asm_gen += 1;
        let generation = self.asm_gen;
        self.asm_loading = true;
        let engine = self.engine.clone();
        let tx = self.app_tx.clone();
        self.runtime.spawn(async move {
            let result = engine.generate_asm(&file, &[]).await;
            let _ = tx.send(AppEvent::AsmReady { generation, result });
        });
    }

    fn on_asm_ready(&mut self, generation: u64, result: AsmResult) {
        if generation != self.asm_gen {
            return; // superseded by a newer refresh
        }
        self.asm_loading = false;
        if !result.success {
            if let Some(e) = &result.error {
                self.status_line(&format!("[forge] asm failed: {e}"));
            }
            return;
        }
        let mut view = crate::asm::AsmView::new(&result.asm, result.asm_to_source);
        // Preserve/re-apply the source-line highlight for the current caret.
        if let Some(src) = self.caret_source_line() {
            view.select_source(src);
        }
        self.asm = Some(view);
        self.sync_asm_selection();
    }

    /// The active tab's caret source line (1-based), for asm cross-highlighting.
    fn caret_source_line(&self) -> Option<u32> {
        self.editor.active_tab().map(|t| t.caret_point().row as u32 + 1)
    }

    /// Sync the ASM overlay's highlight to the source caret and scroll the first
    /// mapped asm line into view (source→asm direction, §6).
    pub fn sync_asm_selection(&mut self) {
        if !self.asm_visible {
            return;
        }
        let Some(src) = self.caret_source_line() else {
            return;
        };
        let target = if let Some(view) = &mut self.asm {
            view.select_source(src);
            view.first_asm_row_for_source(src)
        } else {
            None
        };
        if let Some(row) = target {
            self.asm_scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Center);
        }
    }

    /// An asm row was clicked (§6): highlight its source counterpart and scroll
    /// the source viewer to it (asm→src also scrolls).
    pub fn asm_click(&mut self, asm_row0: usize) {
        let src = self.asm.as_mut().and_then(|view| {
            let s = view.source_for_asm(asm_row0);
            if let Some(s) = s {
                view.select_source(s);
            }
            s
        });
        if let Some(s) = src {
            self.reveal_line(s as usize);
        }
    }

    /// Auto-refresh the ASM 1.5s after a buffer edit while the overlay is visible
    /// (§6). Debounced via a generation guard on the async wake.
    fn schedule_asm_refresh(&mut self, cx: &mut Context<Self>) {
        if !self.asm_visible {
            return;
        }
        self.asm_gen += 1;
        let generation = self.asm_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |app, cx| {
                // Only fire if no newer edit/toggle superseded this wake.
                if app.asm_gen == generation && app.asm_visible {
                    app.refresh_asm(cx);
                }
            });
        })
        .detach();
    }

    // ── Breakpoints (§4.6) ────────────────────────────────────────────────────

    /// Toggle a breakpoint on the active file at 1-based `line` (gutter click).
    /// Live-syncs the LLDB driver when a session exists, and persists the set.
    pub fn toggle_breakpoint(&mut self, line: u32) {
        let Some(path) = self.active_file.clone() else {
            return;
        };
        let file = path.display().to_string();
        let change = self.breakpoints.toggle(&file, line);
        // Live-sync the driver (set_breakpoint / remove_breakpoint, §4.6).
        if self.driver.is_some() {
            let driver = self.driver.clone().unwrap();
            let f = file.clone();
            self.runtime.spawn(async move {
                let d = driver.lock().await;
                match change {
                    crate::debug::BreakpointChange::Added => d.set_breakpoint(&f, line).await,
                    crate::debug::BreakpointChange::Removed => d.remove_breakpoint(&f, line).await,
                }
            });
        }
    }

    /// Whether `line` (1-based) has a breakpoint in the active file.
    pub fn is_breakpoint(&self, line: u32) -> bool {
        match &self.active_file {
            Some(p) => self.breakpoints.is_set(&p.display().to_string(), line),
            None => false,
        }
    }

    // ── Benchmarks (§5.4) ─────────────────────────────────────────────────────

    /// Begin naming a benchmark from HISTORY run `run_index` (the ⚑ flag). Shows
    /// the inline input prefilled `#<run> <flags>` (§5.4).
    pub fn begin_benchmark(&mut self, run_index: usize) {
        // Current custom flags aren't wired to a UI field yet — use the last
        // build's flags surrogate (empty for the default preset).
        let flags = String::new();
        self.bench_naming = Some(BenchNaming {
            run_index,
            buffer: crate::benchmark::default_name(run_index, &flags),
            flags,
        });
    }

    /// Commit the in-flight benchmark name (Enter), snapshotting the run's stats.
    pub fn commit_benchmark(&mut self) {
        let Some(naming) = self.bench_naming.take() else {
            return;
        };
        let name = naming.buffer.trim().to_string();
        if name.is_empty() {
            return;
        }
        let rec = self.run_history.iter().find(|r| r.n == naming.run_index);
        let (duration, peak) = match rec {
            Some(r) => (r.duration_ms as f64, r.peak),
            None => return,
        };
        self.benchmarks.push(crate::benchmark::Benchmark {
            name,
            flags: naming.flags,
            duration,
            peak_allocation: peak,
            alloc_count: self.mem.alloc_count as u64,
            timestamp: crate::benchmark::now_ms(),
        });
    }

    /// Cancel the in-flight benchmark name (Esc).
    pub fn cancel_benchmark(&mut self) {
        self.bench_naming = None;
    }

    /// Delete a saved benchmark by index (× button).
    pub fn delete_benchmark(&mut self, index: usize) {
        if index < self.benchmarks.len() {
            self.benchmarks.remove(index);
        }
    }

    /// The latest completed run's duration (ms) — the benchmark delta baseline.
    pub fn latest_run_ms(&self) -> Option<f64> {
        self.run_history.last().map(|r| r.duration_ms as f64)
    }

    /// Apply one captured keystroke to the benchmark-name input. Enter commits,
    /// Esc cancels, printable chars append, Backspace pops. Returns true if consumed.
    pub fn bench_key(&mut self, ks: &gpui::Keystroke) -> bool {
        if self.bench_naming.is_none() {
            return false;
        }
        let m = ks.modifiers;
        match ks.key.as_str() {
            "enter" => self.commit_benchmark(),
            "escape" => self.cancel_benchmark(),
            "backspace" => {
                if let Some(n) = &mut self.bench_naming {
                    n.buffer.pop();
                }
            }
            _ => {
                let printable =
                    ks.key_char.is_some() && !m.platform && !m.control && !m.alt && !m.function;
                if printable {
                    let ch = ks.key_char.clone().unwrap();
                    if let Some(n) = &mut self.bench_naming {
                        n.buffer.push_str(&ch);
                    }
                } else {
                    return false;
                }
            }
        }
        true
    }

    // ── Per-workspace UI persistence (§1.2) ───────────────────────────────────

    /// Snapshot the current UI state into the persisted `ui` blob shape.
    fn build_ui_state(&self) -> crate::workspace_state::WorkspaceUi {
        let open_tabs = self
            .editor
            .tabs
            .iter()
            .map(|t| crate::workspace_state::TabState {
                path: t.path.display().to_string(),
                is_dirty: t.buffer.is_dirty(),
            })
            .collect();
        crate::workspace_state::WorkspaceUi {
            open_tabs,
            active_tab_index: self.editor.active.map(|i| i as i64),
            file_tree_visible: Some(self.sidebar_tab == SidebarTab::Files),
            terminal_visible: Some(self.output_visible),
            memory_bar_visible: Some(true),
            terminal_height: None,
            breakpoints: self.breakpoints.to_map(),
            benchmarks: self.benchmarks.clone(),
            ai_completion_enabled: Some(self.ai_completion_enabled),
        }
    }

    /// Persist the UI blob immediately (merge-preserving other keys). Used by the
    /// headless smoke/tests; the GUI goes through [`schedule_ui_save`].
    pub fn save_ui_state(&self) {
        crate::workspace_state::save(&self.workspace_root, &self.build_ui_state());
    }

    /// Debounced UI-blob save (1500ms, `state.ts:130`). Coalesces bursts via a
    /// generation guard; the merge in `workspace_state::save` preserves any
    /// stickyNotes blob the Electron app left in the same file.
    /// Called from every UI mutation that changes a persisted key.
    pub fn schedule_ui_save(&mut self, cx: &mut Context<Self>) {
        self.ui_save_gen += 1;
        let generation = self.ui_save_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |app, _| {
                if app.ui_save_gen == generation {
                    app.save_ui_state();
                }
            });
        })
        .detach();
    }
}

/// Flatten LSP hover contents to plain text (no markdown rendering yet — E2
/// deferral). Handles the scalar / array / markup encodings.
fn flatten_hover(hover: &forge_lsp::Hover) -> String {
    match &hover.contents {
        HoverContents::Scalar(m) => marked_string_text(m),
        HoverContents::Array(ms) => ms
            .iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n"),
        HoverContents::Markup(mc) => mc.value.clone(),
    }
}

fn marked_string_text(m: &lsp_types::MarkedString) -> String {
    match m {
        lsp_types::MarkedString::String(s) => s.clone(),
        lsp_types::MarkedString::LanguageString(ls) => ls.value.clone(),
    }
}

/// Recover a filesystem path from a `file://` URI (lsp-types 0.97 models URIs
/// with `fluent_uri`, which has no `to_file_path`). Reverses percent-encoding.
fn path_from_uri(uri: &lsp_types::Uri) -> PathBuf {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://").unwrap_or(s);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

// ── IME / text input (E2) ─────────────────────────────────────────────────────
impl EntityInputHandler for JadeApp {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let tab = self.editor.active_tab()?;
        let text = tab.buffer.to_string();
        let s = editor_view::doc_utf16_to_byte(&tab.buffer, range_utf16.start).min(text.len());
        let e = editor_view::doc_utf16_to_byte(&tab.buffer, range_utf16.end).min(text.len());
        let (s, e) = (s.min(e), s.max(e));
        *adjusted = Some(
            editor_view::byte_to_doc_utf16(&tab.buffer, s)
                ..editor_view::byte_to_doc_utf16(&tab.buffer, e),
        );
        Some(text[s..e].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let tab = self.editor.active_tab()?;
        let sel = tab.buffer.selection();
        let range = editor_view::byte_to_doc_utf16(&tab.buffer, sel.start())
            ..editor_view::byte_to_doc_utf16(&tab.buffer, sel.end());
        Some(UTF16Selection {
            range,
            reversed: sel.head < sel.anchor,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let tab = self.editor.active_tab()?;
        let m = tab.marked.clone()?;
        Some(
            editor_view::byte_to_doc_utf16(&tab.buffer, m.start)
                ..editor_view::byte_to_doc_utf16(&tab.buffer, m.end),
        )
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.editor.active_tab_mut() {
            tab.marked = None;
        }
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let record = self.editor.active_tab_mut().map(|tab| {
            let range = range_utf16
                .map(|r| {
                    editor_view::doc_utf16_to_byte(&tab.buffer, r.start)
                        ..editor_view::doc_utf16_to_byte(&tab.buffer, r.end)
                })
                .or_else(|| tab.marked.clone())
                .unwrap_or_else(|| tab.buffer.selection().range());
            tab.marked = None;
            tab.buffer.edit(range, text)
        });
        if let Some(record) = record {
            self.after_edit(record, cx);
        }
        if text.is_empty() {
            self.dismiss_popups();
        } else {
            self.on_text_inserted(text, cx);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let record = self.editor.active_tab_mut().map(|tab| {
            let range = range_utf16
                .map(|r| {
                    editor_view::doc_utf16_to_byte(&tab.buffer, r.start)
                        ..editor_view::doc_utf16_to_byte(&tab.buffer, r.end)
                })
                .or_else(|| tab.marked.clone())
                .unwrap_or_else(|| tab.buffer.selection().range());
            let start = range.start;
            let rec = tab.buffer.edit(range, new_text);
            tab.marked = if new_text.is_empty() {
                None
            } else {
                Some(start..start + new_text.len())
            };
            rec
        });
        if let Some(record) = record {
            self.after_edit(record, cx);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let tab = self.editor.active_tab()?;
        let byte = editor_view::doc_utf16_to_byte(&tab.buffer, range_utf16.start);
        let p = tab.buffer.offset_to_point(byte);
        let top = self.editor_scroll_top();
        let text_left = f32::from_bits(self.editor_text_left.load(Ordering::Relaxed));
        let cw = self.char_w();
        // Anchor at the caret cell, using the captured text-left and the row's
        // offset from the current scroll top (best-effort IME candidate position).
        let x = px(text_left + editor_view::col_to_px(p.col, cw));
        let y = element_bounds.origin.y + px((p.row.saturating_sub(top)) as f32 * LINE_H);
        Some(Bounds::new(
            gpui::point(x, y),
            gpui::size(px(2.0), px(LINE_H)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// Parse the first `path:line:col` triple in sanitizer output into a 1-based
/// error line (app.ts:1169 `/(\S+):(\d+):\d+/`). The `\S+` path segment must be
/// non-empty and the two numeric groups colon-separated.
fn parse_error_line(sanitizer_output: &str) -> Option<u32> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(\S+):(\d+):\d+").unwrap());
    re.captures(sanitizer_output)
        .and_then(|c| c.get(2))
        .and_then(|m| m.as_str().parse::<u32>().ok())
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
        if self.output_visible && !self.runtime_visible && self.bottom_view == BottomView::Terminal {
            self.ensure_terminal();
        }
        let term_handle = self
            .term_focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();

        // Ensure the editor focus handle exists so the center surface can take
        // keyboard + IME input (created lazily like the terminal's).
        if self.editor_focus.is_none() {
            self.editor_focus = Some(cx.focus_handle());
        }
        // 530ms caret-blink driver (GUI only — headless assemble never renders).
        // Toggles the phase while the editor owns focus; caret_activity() holds
        // it solid through typing/navigation.
        if !self.blink_task_running {
            self.blink_task_running = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(530))
                        .await;
                    let alive = this.update(cx, |app, cx| {
                        let idle = app.now_ms().saturating_sub(app.caret_last_active);
                        let show = if idle < 530 { true } else { !app.caret_blink_show };
                        if show != app.caret_blink_show {
                            app.caret_blink_show = show;
                            cx.notify();
                        }
                    });
                    if alive.is_err() {
                        break; // window closed
                    }
                }
            })
            .detach();
        }

        // A file was just opened: hand the editor keyboard focus (open sites have
        // no Window; this render does). Skipped while an overlay owns input.
        if self.pending_editor_focus {
            self.pending_editor_focus = false;
            if self.quick_open.is_none() {
                if let Some(h) = &self.editor_focus {
                    h.focus(window, cx);
                }
            }
        }

        // Benchmark-name input focus (§5.4): focus it while naming so keystrokes
        // reach the inline input instead of the editor/terminal.
        let bench_handle = self.bench_focus.get_or_insert_with(|| cx.focus_handle()).clone();
        if self.bench_naming.is_some() && !bench_handle.is_focused(window) {
            bench_handle.focus(window, cx);
        }

        let mut root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(theme.bg))
            .text_color(rgb(theme.text))
            .font_family(crate::fonts::mono_family()) // bundled JetBrains Mono, else Menlo
            .text_sm()
            // Global ⌘P: toggle Quick Open (§5.7). Root-level so it fires whether
            // or not a child (terminal, overlay) holds focus — key events bubble
            // to the window root when nothing else consumes them.
            .on_key_down(cx.listener(|app, ev: &KeyDownEvent, _win, cx| {
                // Global shortcuts (§3): fire whether or not a child holds focus,
                // since unconsumed key events bubble to the window root.
                let ks = &ev.keystroke;
                let m = ks.modifiers;
                match ks.key.as_str() {
                    "p" if m.platform => {
                        app.toggle_quick_open();
                        cx.notify();
                    }
                    // ⌘⇧O / ⌘O (both unbound in the editor path) open the folder
                    // picker (inventory §2). Fires whether or not a workspace is
                    // already open, so you can switch folders any time.
                    "o" if m.platform => {
                        app.prompt_open_project(cx);
                    }
                    // ⌘⇧A toggles the ASM viewer (also handled in the focused
                    // editor's key path; this catches the case where it isn't).
                    "a" if m.platform && m.shift => {
                        app.toggle_asm(cx);
                        cx.notify();
                    }
                    // Debug stepping (§3): F5 continue, F10 over, F11 into, ⇧F11 out.
                    "f5" => {
                        app.debug_continue();
                        cx.notify();
                    }
                    "f10" => {
                        app.debug_step_over();
                        cx.notify();
                    }
                    "f11" if m.shift => {
                        app.debug_step_out();
                        cx.notify();
                    }
                    "f11" => {
                        app.debug_step_into();
                        cx.notify();
                    }
                    _ => {}
                }
            }))
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
                    .child(runtime_sidebar(self, cx, &theme, bench_handle)),
            );

        // Debug panel docks above the terminal, hiding it while a session is
        // active (§5.8); else the terminal/output strip shows when visible.
        if self.debug_visible {
            root = root.child(debug_panel::render(self, cx));
        } else if self.output_visible && !self.runtime_visible {
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

        // §5.7 Quick Open overlay: centered over the editor area, focused so its
        // captured-keystroke buffer receives input.
        if self.quick_open.is_some() {
            let focus = self
                .quick_open_focus
                .get_or_insert_with(|| cx.focus_handle())
                .clone();
            if !focus.is_focused(window) {
                focus.focus(window, cx);
            }
            let overlay = crate::panels::quick_open::overlay(self, focus, cx);
            root = root.child(overlay);
        }
        root
    }
}

/// The floating-card drop shadow (§2 "the look"; main.css `--shadow-sm`,
/// main.css:85 / :2342-2344): `0 1px 2px rgba(0,0,0,0.25)`. Applied to the
/// sidebar / editor / runtime / terminal cards so both themes share one subtle
/// elevation.
fn card_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.25)).blur_radius(px(2.))]
}

fn action_bar(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    let can_run = app.can_run();
    let build_label = if app.building { "Building…" } else { "Build" }.to_string();
    let run_label = if app.running { "Running…" } else { "Run" }.to_string();
    let ai_active = matches!(app.ai_status.state, AiState::Ready);

    // Left cluster (screenshot ref / main.css:222-243): compact icon-only
    // toggles — no chip boxes or labels.
    let terminal_active = app.output_visible && app.bottom_view == BottomView::Terminal;
    let toggles = div()
        .flex()
        .items_center()
        .gap_1()
        .child(icon_btn("tgl-files", "panel-left", theme, !app.sidebar_collapsed, cx, |a, _| {
            a.toggle_sidebar()
        }))
        .child(icon_btn("tgl-terminal", "terminal", theme, terminal_active, cx, |a, cx| {
            if a.output_visible && a.bottom_view == BottomView::Terminal {
                a.action_toggle_output();
            } else {
                a.set_bottom_view(BottomView::Terminal);
            }
            a.schedule_ui_save(cx); // terminalVisible (§1.2)
        }))
        .child(icon_btn("tgl-flow", "corner-down-right", theme, app.flow_visible, cx, |a, _| {
            a.action_toggle_flow()
        }))
        .child(icon_btn("tgl-runtime", "gauge", theme, app.runtime_visible, cx, |a, _| {
            a.action_toggle_runtime()
        }));

    // Diagnostic pills (always visible, zero included — screenshot center-left).
    let (errs, warns, infos) = app.active_diag_counts();
    let diag_badges = div()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .child(diag_pill("circle-x", errs, theme.red))
        .child(diag_pill("triangle-alert", warns, theme.amber))
        .child(diag_pill("info", infos, theme.muted));

    let right_group = div()
        .flex()
        .items_center()
        .gap_2()
        // ASM viewer (§6, ⌘⇧A): plain icon+label, no box.
        .child(flat_btn("chip-asm", "code", "ASM", theme, app.asm_visible, false, cx, |a, cx| {
            a.toggle_asm(cx)
        }))
        // Build / Run: outlined accent pills (screenshot ref).
        .child(pill_btn("btn-build", "hammer", build_label, theme.accent, theme, app.building, cx, |a, _| {
            a.action_build()
        }))
        .child(pill_btn("btn-run", "play", run_label, theme.text, theme, app.running || !can_run, cx, |a, _| {
            a.action_run()
        }))
        .child(flat_btn("btn-debug", "bug", "Debug", theme, app.debugging, app.building, cx, |a, _| {
            a.action_debug()
        }))
        .child(flat_btn("btn-stop", "square", "Stop", theme, false, false, cx, |a, _| {
            a.action_stop()
        }))
        // Trailing icon-only cluster: AI backend · ghost text · multiline ·
        // theme · open-folder.
        .child(icon_btn("btn-ai", "sparkles", theme, ai_active, cx, |a, _| a.action_ai()))
        .child(icon_btn(
            "btn-ai-ghost",
            if app.ai_completion_enabled { "eye" } else { "eye-off" },
            theme,
            app.ai_completion_enabled,
            cx,
            |a, cx| {
                a.action_toggle_ai_completion();
                a.schedule_ui_save(cx); // aiCompletionEnabled (§1.2)
            },
        ))
        .child(icon_btn("btn-ai-multi", "layers", theme, app.ai_multiline, cx, |a, _| {
            a.action_toggle_ai_multiline()
        }))
        .child(icon_btn(
            "btn-theme",
            if theme.is_light { "sun" } else { "moon" },
            theme,
            false,
            cx,
            |a, _| a.action_theme(),
        ))
        // Open Folder (inventory §2, ⌘⇧O): native directory picker.
        .child(icon_btn("btn-open-folder", "folder-open", theme, false, cx, |a, cx| {
            a.prompt_open_project(cx)
        }));

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(38.))
        .pl(px(80.)) // clear the traffic lights (hiddenInset title bar; main.css:222-243)
        .pr(px(12.))
        .bg(rgb(theme.panel))
        .border_b_1()
        .border_color(rgb(theme.border))
        // Window-drag region: empty parts of the bar drag the window; the
        // buttons' own click hitboxes take priority (`no-drag` equivalent).
        .window_control_area(WindowControlArea::Drag)
        .child(toggles)
        .child(diag_badges)
        .child(right_group)
}

/// An icon-only action-bar button (no box, no label): muted at rest, accent
/// when `active`, hover wash. 26px square hit target around a 15px glyph.
fn icon_btn(
    id: &'static str,
    icon: &'static str,
    theme: &Theme,
    active: bool,
    cx: &mut Context<JadeApp>,
    f: impl Fn(&mut JadeApp, &mut Context<JadeApp>) + 'static,
) -> impl IntoElement {
    let color = if active { theme.accent } else { theme.muted };
    let hover_bg = theme.border;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(26.))
        .h(px(26.))
        .rounded_md()
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(hover_bg)))
        .on_click(cx.listener(move |a, _ev, _win, cx| {
            f(a, cx);
            cx.notify();
        }))
        .child(crate::assets::ui_icon(icon, 15., color))
}

/// A flat icon+label button (no border/fill): text-colored, hover wash.
#[allow(clippy::too_many_arguments)]
fn flat_btn(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
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
    let hover_bg = theme.border;
    let el = div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .text_color(rgb(color))
        .child(crate::assets::ui_icon(icon, 14., color))
        .child(label);
    if disabled {
        el
    } else {
        el.cursor_pointer()
            .hover(move |s| s.bg(rgb(hover_bg)))
            .on_click(cx.listener(move |a, _ev, _win, cx| {
                f(a, cx);
                cx.notify();
            }))
    }
}

/// An outlined pill button (Build/Run): 1px `color` border + `color` icon and
/// text on the panel background, hover wash. `busy` mutes it and drops clicks.
#[allow(clippy::too_many_arguments)]
fn pill_btn(
    id: &'static str,
    icon: &'static str,
    label: String,
    color: u32,
    theme: &Theme,
    busy: bool,
    cx: &mut Context<JadeApp>,
    f: impl Fn(&mut JadeApp, &mut Context<JadeApp>) + 'static,
) -> impl IntoElement {
    let color = if busy { theme.muted } else { color };
    let hover_bg = theme.border;
    let el = div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(color))
        .text_xs()
        .text_color(rgb(color))
        .child(crate::assets::ui_icon(icon, 14., color))
        .child(label);
    if busy {
        el
    } else {
        el.cursor_pointer()
            .hover(move |s| s.bg(rgb(hover_bg)))
            .on_click(cx.listener(move |a, _ev, _win, cx| {
                f(a, cx);
                cx.notify();
            }))
    }
}

/// One always-visible diagnostic pill: colored icon + count on a faint tinted
/// capsule (screenshot ref — counts show even at zero).
fn diag_pill(icon: &'static str, count: usize, color: u32) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py(px(2.))
        .rounded_full()
        .bg(gpui::Rgba {
            r: ((color >> 16) & 0xff) as f32 / 255. * 0.15,
            g: ((color >> 8) & 0xff) as f32 / 255. * 0.15,
            b: (color & 0xff) as f32 / 255. * 0.15,
            a: 1.0,
        })
        .text_color(rgb(color))
        .child(crate::assets::ui_icon(icon, 13., color))
        .child(format!("{count}"))
}

/// Left panel: the file-tree card (deliverable §2, floating-card look §2). When
/// collapsed it shrinks to a 28px clickable strip showing a stacked "FILES" label
/// that reopens it (app.ts:449-459, main.css:481-495).
fn left_panel(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    // Collapsed: a 28px strip with a vertical "FILES" label (GPUI has no cheap
    // text-rotation, so the letters are stacked); clicking anywhere reopens it.
    if app.sidebar_collapsed {
        let mut label = div().flex().flex_col().items_center().gap(px(1.)).pt(px(12.));
        for ch in "FILES".chars() {
            label = label.child(
                div()
                    .text_color(rgb(theme.muted))
                    .text_xs()
                    .child(ch.to_string()),
            );
        }
        return div()
            .id("sidebar-collapsed")
            .flex()
            .flex_col()
            .items_center()
            .w(px(28.))
            .rounded_lg()
            .bg(rgb(theme.panel))
            .border_1()
            .border_color(rgb(theme.border))
            .shadow(card_shadow())
            .overflow_hidden()
            .cursor_pointer()
            .hover(|s| s.bg(rgb(theme.border)))
            .on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
                a.toggle_sidebar();
                cx.notify();
            }))
            .child(label)
            .into_any_element();
    }

    // FILES | STRUCTURE tab switcher over the tree or the symbol outline (§5.5).
    let body = match app.sidebar_tab {
        SidebarTab::Files => file_tree::render(app, cx).into_any_element(),
        SidebarTab::Structure => structure_panel::render(app, cx).into_any_element(),
    };
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
        .shadow(card_shadow())
        .overflow_hidden()
        .child(structure_panel::tab_switcher(app, cx, theme))
        .child(body)
        .into_any_element()
}

/// Center: the tab strip + read-only code viewer (deliverables §3, §5), replacing
/// the placeholder. With no workspace open (§2) the welcome overlay covers this
/// area instead.
fn center_content(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    let base = div()
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .rounded_lg()
        .bg(rgb(theme.bg))
        .border_1()
        .border_color(rgb(theme.border))
        .shadow(card_shadow()) // floating-card look (§2, main.css:2342-2344)
        .overflow_hidden();

    // No workspace: the card hosts the welcome overlay instead of the editor.
    if !app.workspace_opened {
        return base.child(welcome_overlay(cx, theme));
    }

    let mut center = base
        .child(code_view::tab_strip(app, cx, theme))
        .child(code_view::render(app, cx));
    // §6 ASM viewer: right-half overlay over the editor when toggled on.
    if app.asm_visible {
        center = center.child(asm_view::overlay(app, cx));
    }
    center
}

/// Welcome overlay shown when no workspace is open (inventory §2, `app.ts:54-81`):
/// centered "Jade" title, "Open a folder to get started", an Open Folder button
/// (outline + folder-open icon), and a shortcut-hint row. Mirrors the Electron
/// `#welcome-overlay` styling in GPUI theme colors.
fn welcome_overlay(cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    let hint = |s: &str| {
        div()
            .text_color(rgb(theme.muted))
            .child(s.to_string())
    };
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                // welcome-title
                .child(
                    div()
                        .text_color(rgb(theme.accent))
                        .text_2xl()
                        .child("Jade"),
                )
                // welcome-subtitle
                .child(
                    div()
                        .mt(px(8.))
                        .text_color(rgb(theme.muted))
                        .text_sm()
                        .child("Open a folder to get started"),
                )
                // Open Folder button (outline + folder-open icon).
                .child(
                    div()
                        .id("open-folder-btn")
                        .mt(px(24.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px(px(24.))
                        .py(px(8.))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(theme.border))
                        .text_color(rgb(theme.text))
                        .text_sm()
                        .cursor_pointer()
                        .on_click(cx.listener(|a: &mut JadeApp, _ev, _win, cx| {
                            a.prompt_open_project(cx);
                        }))
                        .child(crate::assets::ui_icon("folder-open", 14., theme.text))
                        .child("Open Folder"),
                )
                // welcome-shortcuts hint row.
                .child(
                    div()
                        .mt(px(32.))
                        .flex()
                        .gap(px(20.))
                        .text_xs()
                        .child(hint("⌘B File tree"))
                        .child(hint("⌘` Terminal"))
                        .child(hint("⌘E Flow arrows"))
                        .child(hint("⌘S Save")),
                ),
        )
}

/// The right sidebar (gauge toggle): RUNTIME graphs over TRAINING + TELEMETRY.
/// Fully hidden (zero-size) when toggled off — the gauge controls the whole
/// sidebar, not just the runtime section (§5.4; user spec 2026-07-15).
fn runtime_sidebar(
    app: &JadeApp,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
    bench_handle: FocusHandle,
) -> gpui::AnyElement {
    if !app.runtime_visible {
        return div().into_any_element();
    }
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
        .shadow(card_shadow()) // floating-card look (§2, main.css:2342-2344)
        .overflow_y_scroll()
        .child(runtime_panel::render(app, bench_handle, cx))
        .child(training_view::render(app, cx))
        .child(telemetry_sidebar::render(app, cx))
        .into_any_element()
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
                .child(
                    div()
                        .flex()
                        .items_center()
                        .text_color(rgb(theme.muted))
                        .child(crate::assets::ui_icon("terminal", 13., theme.muted)),
                )
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
                        .flex()
                        .items_center()
                        .text_color(rgb(theme.muted))
                        .cursor_pointer()
                        .on_click(cx.listener(|a: &mut JadeApp, _ev, _win, cx| {
                            a.action_new_terminal();
                            cx.notify();
                        }))
                        .child(crate::assets::ui_icon("plus", 14., theme.muted)),
                )
                // Minimize (hide the strip).
                .child(
                    div()
                        .id("term-min")
                        .flex()
                        .items_center()
                        .text_color(rgb(theme.muted))
                        .cursor_pointer()
                        .on_click(cx.listener(|a: &mut JadeApp, _ev, _win, cx| {
                            a.action_toggle_output();
                            cx.notify();
                        }))
                        .child(crate::assets::ui_icon("minus", 14., theme.muted)),
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
        output_view(app, theme, cx).into_any_element()
    };

    // Floating-card treatment (§2; main.css `#terminal-area` padding 6px 6px 0 6px
    // + `.terminal-panel` card): the strip sits in a 6px gutter as a rounded,
    // bordered, shadowed card matching the sidebar / editor / runtime cards.
    div()
        .flex()
        .flex_col()
        .flex_none()
        .px(px(6.))
        .pt(px(6.))
        .child(
            div()
                .id("bottom-panel")
                .flex()
                .flex_col()
                .h(px(220.))
                .w_full()
                .rounded_lg()
                .bg(rgb(theme.bg))
                .border_1()
                .border_color(rgb(theme.border))
                .shadow(card_shadow())
                .overflow_hidden()
                .child(header)
                .child(body),
        )
}

/// The OUTPUT scrollback view (deliverable §4): capped scrollback, monospace,
/// muted, newest visible. ANSI already stripped on ingest. Diagnostic lines
/// (`/abs/path:line:col: …`) are clickable — jump to the file + line — and
/// tinted by severity, so a failed build is navigable like the Electron app's
/// error list.
fn output_view(app: &JadeApp, theme: &Theme, cx: &mut Context<JadeApp>) -> impl IntoElement {
    // Render the last ~200 lines — enough to fill the strip without a huge tree.
    let start = app.output.len().saturating_sub(200);
    let mut list = div().flex().flex_col();
    for (n, line) in app.output[start..].iter().enumerate() {
        let text = if line.is_empty() { " ".to_string() } else { line.clone() };
        let color = if line.contains(" error: ") {
            theme.red
        } else if line.contains(" warning: ") {
            theme.amber
        } else {
            theme.muted
        };
        let mut row = div()
            .id(("out-line", n))
            .text_color(rgb(color))
            .text_xs()
            .child(text);
        if let Some((path, lineno)) = parse_jump_target(line) {
            row = row.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(move |app: &mut JadeApp, _ev, _w, cx| {
                    app.open_file(path.clone());
                    app.reveal_line(lineno as usize);
                    cx.notify();
                }),
            );
        }
        list = list.child(row);
    }
    div()
        .id("output-panel")
        .flex_1()
        .w_full()
        .p(px(8.))
        .overflow_y_scroll()
        .child(list)
}

/// Parse a `/abs/path:line:col:` diagnostic prefix from an output line (as
/// emitted by `on_build_done`), returning the jump target. Lines that don't
/// look like one (relative paths, missing numbers) return `None`.
fn parse_jump_target(line: &str) -> Option<(PathBuf, u32)> {
    let t = line.trim_start();
    if !t.starts_with('/') {
        return None;
    }
    let mut colons = t.match_indices(':');
    let (p1, _) = colons.next()?;
    let (p2, _) = colons.next()?;
    let (p3, _) = colons.next()?;
    let lineno: u32 = t[p1 + 1..p2].parse().ok()?;
    let _col: u32 = t[p2 + 1..p3].parse().ok()?;
    let path = PathBuf::from(&t[..p1]);
    (lineno > 0).then_some((path, lineno))
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

#[cfg(test)]
mod output_jump_tests {
    use super::parse_jump_target;
    use std::path::PathBuf;

    #[test]
    fn parses_clang_diagnostic_prefix() {
        let line = "  /Users/x/proj/main.cpp:979:9: error: use of undeclared identifier";
        assert_eq!(
            parse_jump_target(line),
            Some((PathBuf::from("/Users/x/proj/main.cpp"), 979))
        );
    }

    #[test]
    fn rejects_non_diagnostic_lines() {
        assert_eq!(parse_jump_target("[forge] Build failed (5 error(s))"), None);
        assert_eq!(parse_jump_target("main.cpp:1:1: error: relative path"), None);
        assert_eq!(parse_jump_target("/Users/x/notes.txt: no numbers"), None);
        assert_eq!(parse_jump_target("/Users/x/a.cpp:0:1: zero line"), None);
    }
}
