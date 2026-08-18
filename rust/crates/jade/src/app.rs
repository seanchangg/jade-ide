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

use jade_ai::{AiModelId, AiState, AiStatus, InfillRequest, InlineCompletionBackend};
use jade_build::{
    parse_alloc_free, parse_heap_summary, parse_scalar, parse_timing, AsmResult, AtosSymbolicator,
    BuildEngine, BuildResult, CompileRequest, MemoryEvent, RunConfig, RunEvent, RunResult,
    INTERPOSE_DYLIB, PROBE_DYLIB,
};
use jade_buffer::{Point, Selection};
use jade_debug::{DebugEvent, LldbDriver, LocalVariable};
use jade_lsp::{
    active_signature_hint, CompletionItem, DidChange, HoverContents, LspClient, LspEvent, LspHandle,
    SignatureHint, TextDocumentSyncKind,
};
use jade_sysmon::{SystemMonitor, SystemStats};
use jade_telemetry::{Event, Kind, TelemetryServer};
use jade_term::{GridSnapshot, TermEvent, TermId, TermManager};
use gpui::{
    div, prelude::*, px, rgb, Bounds, BoxShadow, ClipboardItem, Context,
    EntityInputHandler, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, PathPromptOptions,
    Pixels, UTF16Selection, Window, WindowControlArea,
};
use serde_json::{Map, Value};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

// Declared here (not in `main.rs`, which this change must not touch) via an
// explicit `#[path]`: the physical file lives at `src/dim_input.rs` but the
// module nests under `crate::app` — see `dim_input`'s doc comment for what it
// implements (the rows×cols shape-hint editor shared by the wg3d toolbar and
// the telemetry sidebar).
#[path = "dim_input.rs"]
pub mod dim_input;

use crate::editor_view::{self, EditorState};
use crate::kumo::{
    self, scale, separator_v, Badge, BadgeVariant, Button, ButtonVariant, Card, DotColor, Heading,
    HeadingLevel, Size as KumoSize, TabBar, TabItem, TabsAppearance, Text as KumoText, TextTone,
};
use crate::highlight::TokenPalette;
use crate::memory_bar::{project, Level, MemoryBarState};
use crate::output::push_output;
use crate::panels::runtime_panel::{self, RunRecord};
use crate::panels::metric_popout::{MetricPopout, MetricSection};
use crate::panels::{
    asm_view, code_view, debug_panel, file_tree, structure_panel, telemetry_sidebar,
    terminal_panel, training_view,
};
use crate::ai_prefs::AiPrefs;
use crate::prefs::TelemetryPrefs;
use crate::quick_open::{self, FileEntry, KeyAction, Match, QuickOpenState};
use crate::registry::{key_of, TelemetryRegistry, DEFAULT_MAX_DIM};
use crate::structure::Symbol;
use crate::theme::Theme;
use crate::run_store::{PendingRun, RunMeta, RunStore, KIND_DEBUG, KIND_RUN};
use crate::training::{TensorFrame, TrainingData};
use crate::wg3d::WeightGrid3D;
use crate::workspace_tree::FileTree;

/// Which view the bottom panel shows. The TERMINAL view is a live shell; the
/// OUTPUT view is the plain `[jade]`/build/run scrollback fallback (see
/// `terminal_panel` for why status lines can't be injected into the shell grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomView {
    Terminal,
    Output,
}

/// What the pre-run panel's Run button launches when confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreRunLaunch {
    Run,
    Debug,
}

/// State of the pre-run tracking panel: Run/Debug open it first so the user
/// picks which timers/buffers to track before the program launches. A short
/// discovery run fills the registry when it's empty.
#[derive(Debug, Clone, Copy)]
pub struct PreRunPanel {
    pub launch: PreRunLaunch,
    /// The discovery child is still running (list is filling live).
    pub discovering: bool,
}

/// How long a discovery run lives once the app is WARM (first timing/scalar
/// arrived — it's past startup and running kernels). Long enough for a
/// training loop to run a few steps, short enough to not feel like a real run.
pub const DISCOVERY_SECS: u64 = 5;

/// Hard ceiling for a scan whose app never warms up. Metal's backend PSO
/// compile of a heavy kernel (an MPP matmul-dense one runs ~25s cold) freezes
/// startup with buffers declared but no command buffer committed yet; decls
/// prove the probe is alive, so the scan waits — up to this long — instead of
/// reporting a half-empty inventory ("0 timers, 20 buffers").
pub const DISCOVERY_HARD_CAP_SECS: u64 = 180;

/// Whether the discovery watchdog should stop the scan now. `warm_for` is the
/// time since the first scalar/timing event of the scan; `has_decls` is
/// whether any probe decl arrived (an instrumented app mid-startup).
fn discovery_should_stop(
    elapsed: Duration,
    warm_for: Option<Duration>,
    has_decls: bool,
) -> bool {
    match warm_for {
        // Warm: give the run a full post-warm window, however long startup took.
        Some(w) => w >= Duration::from_secs(DISCOVERY_SECS),
        // Probe alive but the app is still starting (e.g. stuck compiling
        // pipelines): keep waiting, bounded by the hard cap.
        None if has_decls => elapsed >= Duration::from_secs(DISCOVERY_HARD_CAP_SECS),
        // No telemetry at all: not an instrumented app — old fixed window.
        None => elapsed >= Duration::from_secs(DISCOVERY_SECS),
    }
}

/// Which view the left sidebar shows (§5.5): the FILES tree or the STRUCTURE
/// outline. Toggled by the sidebar tab switcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
    Files,
    Structure,
}

/// A transient corner notification (build success/failure). Toasts self-expire
/// [`TOAST_MS`] after `created_ms`; a sweeper task spawned from `render` drops
/// them and repaints. See [`JadeApp::push_toast`] and the render overlay.
pub struct Toast {
    pub message: String,
    pub kind: ToastKind,
    /// `now_ms()` when the toast was raised; drives the fade-out + expiry.
    pub created_ms: u64,
}

/// Which accent + glyph a [`Toast`] wears.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Error,
}

/// How long a toast stays up before the sweeper removes it (ms).
pub const TOAST_MS: u64 = 4200;

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
    /// A terminal event from `jade-term` (grid damaged → re-snapshot; child
    /// exited → dim `[exited <code>]`). Phase-4 wave 2, §5.2.
    Term(TermEvent),
    /// The workspace file tree changed on disk (debounced fs-watch, §5.1) — the
    /// app re-scans preserving expansion. Carries nothing; the refresh reads disk.
    TreeChanged,
    /// Periodic discovery watchdog tick — the handler decides via
    /// [`discovery_should_stop`] whether to kill the discovery child (its
    /// `RunDone` finishes the cleanup). Ignored when no scan is active.
    DiscoveryTick,
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
    /// A signature-help response for request `generation`, anchored at the caret
    /// `(row,col)` it was requested from. `hint == None` dismisses. Stale drops.
    SignatureHelp {
        generation: u64,
        hint: Option<SignatureHint>,
        anchor: (usize, usize),
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
    /// Telemetry prefs file override. `None` = the real `~/.config/jade/`
    /// path; tests MUST set this — assembled apps save prefs on every
    /// checkbox/bundle change, and an unset override let the headless suites
    /// write into the developer's actual config.
    pub prefs_path: Option<PathBuf>,
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

/// Signature-help state: the active signature label (e.g. `Point(int x, int y)`),
/// the byte range of the active parameter within it (emphasized), and the caret
/// `(row, col)` the hint is anchored under. Shown while filling an argument list.
pub struct SignatureState {
    pub label: String,
    pub active_param: Option<std::ops::Range<usize>>,
    pub anchor: (usize, usize),
}

/// The current AI ghost-text suggestion (§4.11): the (possibly multi-line)
/// completion text and the caret `(row, col)` it is anchored at.
#[derive(Clone)]
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

/// A background project's editor kept in memory across a project switch: its open
/// tabs, dirty buffers, and each tab's remembered scroll (on `OpenTab`), so
/// switching back restores everything instead of reloading pristine files.
struct StashedProject {
    editor: EditorState,
}

pub struct JadeApp {
    pub server: Arc<TelemetryServer>,
    pub registry: TelemetryRegistry,
    pub training: TrainingData,
    /// Persistent run store (`<workspace>/.jade/runs.db`). `None` when SQLite
    /// couldn't open — the RUNS section hides and runs simply aren't recorded.
    pub run_store: Option<RunStore>,
    /// Cached `list_runs` result for the RUNS section (refreshed after every
    /// save/delete/workspace switch — never queried per-frame).
    pub stored_runs: Vec<RunMeta>,
    /// Stored runs currently overlaid on the Loss/Memory charts, in toggle
    /// order (the position picks the overlay color).
    pub run_overlays: Vec<(i64, crate::training::RunData)>,
    /// Launch context captured at Run/Debug start, consumed when the process
    /// exits to write the run record. (`pub(crate)` for interaction_tests.)
    pub(crate) pending_run: Option<PendingRun>,
    /// Pre-run tracking panel (Some while open): pick the timers/buffers to
    /// track before the program launches. Run/Debug route through it.
    pub pre_run: Option<PreRunPanel>,
    /// A short discovery run is in flight (auto-killed after
    /// [`DISCOVERY_SECS`]) filling the registry with timer/buffer names.
    pub discovery_active: bool,
    /// When the current discovery scan launched.
    pub discovery_started: Option<std::time::Instant>,
    /// First scalar/timing of the scan — the app is past startup (see
    /// [`discovery_should_stop`]).
    pub discovery_warm_at: Option<std::time::Instant>,
    /// Any probe decl arrived during the scan.
    discovery_decls: bool,
    /// Launch deferred until the discovery child exits (Run hit mid-scan).
    pending_launch: Option<PreRunLaunch>,
    /// Focus handle for the pre-run overlay (Esc/Enter), created lazily.
    pub pre_run_focus: Option<FocusHandle>,
    /// Timer bundles: member samples aggregate into one synthetic per-cycle
    /// series (defs persist in `workspace.json`; see `timer_groups.rs`).
    pub timer_groups: crate::timer_groups::GroupAggregator,
    /// Pre-run panel bundle staging: member names picked for a new group.
    pub group_staging: Vec<String>,
    /// Captured-keystroke buffer for the new bundle's name.
    pub group_name_input: String,
    /// Pre-run panel buffer filter (type-to-search, Esc clears).
    pub buffer_search: String,
    pub prefs: TelemetryPrefs,
    pub theme: Theme,
    /// 3D weight-grid overlay (§7.2). Owns its own 64-frame ring per buffer,
    /// fed from the telemetry apply path below and rendered only while visible.
    pub wg3d: WeightGrid3D,
    /// The overlay's Metal renderer (the WebGL-engine port), created lazily on
    /// first visible render; `None` + failed=true → CPU-painter fallback.
    #[cfg(target_os = "macos")]
    wg3d_gpu: Option<crate::wg3d::metal::MetalWg3d>,
    #[cfg(target_os = "macos")]
    wg3d_gpu_failed: bool,
    /// Tensor-preview textures by buffer name, baked once per NEW frame in
    /// [`ensure_preview_images`](Self::ensure_preview_images) and drawn as one
    /// quad each by the training view (TS `drawHeatmap` architecture).
    pub preview_images: HashMap<String, crate::panels::training_view::PreviewImage>,
    /// Pop-out metric windows by section (recording aid, §7.1): the training
    /// view's "⧉" buttons open Loss / Memory / Kernel-time in their own
    /// resizable window. A stale handle (window closed) is replaced on the
    /// next open; an open window is re-focused instead of duplicated.
    metric_popouts: HashMap<MetricSection, gpui::WindowHandle<MetricPopout>>,

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
    /// Rows scrolled up into terminal scrollback (0 = pinned to the live
    /// bottom). Scroll-wheel-up increases it; typing / new snapshots pin back.
    pub term_scroll_back: usize,
    /// Mouse selection over the terminal grid, in logical (row, col) cells of
    /// the combined `scrollback ++ viewport` buffer. ⌘C copies it as text.
    pub term_sel: Option<crate::panels::terminal_panel::TermSelection>,
    /// True while the left button is down extending `term_sel`.
    pub term_sel_dragging: bool,
    /// Terminal body origin in window px (`x.to_bits()<<32 | y.to_bits()`),
    /// written by the resize canvas each paint so mouse listeners can map
    /// window coordinates to grid cells (same trick as `editor_text_left`).
    pub term_origin: Arc<std::sync::atomic::AtomicU64>,
    /// Height of the bottom panel card in px (drag the top edge to resize).
    pub bottom_height: f32,
    /// While dragging the bottom panel's resize handle: `(mouse_y_at_start,
    /// height_at_start)`. `None` when not resizing.
    pub bottom_resize: Option<(f32, f32)>,

    // ── Code-viewing vertical (deliverables §1-§5) ────────────────────────────
    /// Scanned workspace file tree (left panel). `None` if the root is unreadable.
    pub tree: Option<FileTree>,
    /// The last file-tree row the user clicked (directory or file). It marks the
    /// row in the panel and gives a new terminal its cwd (see [`Self::terminal_cwd`]).
    /// `None` until the user clicks a row, and cleared on a project switch.
    pub tree_selection: Option<PathBuf>,
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
    /// The jade include dir passed to clangd as a fallback `-I`, if it exists.
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
    /// Signature-help hint (parameters of the call being filled in). `Some` while
    /// the caret is inside an argument list that clangd could resolve.
    pub signature: Option<SignatureState>,
    /// Monotonic signature-help generation (supersede stale responses).
    signature_gen: u64,
    /// Count of signature-help trigger attempts (before the LSP guard) — a test
    /// seam to prove the `(` / `,` keystroke routing reaches the request path.
    pub sig_help_requests: u64,

    // ── AI ghost text / editor extras (Phase-3b E3) ───────────────────────────
    /// Whether ghost text is offered (workspace UI toggle `aiCompletionEnabled`,
    /// §4.11). Distinct from the backend being Ready — both must hold.
    pub ai_completion_enabled: bool,
    /// Multiline ghost mode (`aiMultiline`, §4.11): 6-line vs single-line.
    pub ai_multiline: bool,
    /// Managed-model tier the AI menu selects (`aiModel`): Fast/Balanced/Best.
    /// Applied via [`jade_ai::InlineCompletionBackend::set_model`] and persisted
    /// globally in [`ai_prefs`](Self::ai_prefs).
    pub ai_model: AiModelId,
    /// Whether the sparkle AI settings menu (completion/multiline/model) is open.
    pub ai_menu_open: bool,
    /// Global (cross-workspace) AI prefs — the model tier + multi-line mode —
    /// mirrored here so a menu change can rewrite `~/.config/jade/ai.json`.
    pub ai_prefs: AiPrefs,
    /// The current ghost suggestion, if any.
    pub ghost: Option<GhostState>,
    /// Monotonic ghost-request generation (supersede stale `/infill` responses).
    ghost_gen: u64,
    /// 48-entry raw-suggestion cache with typed-through hits (§4.11).
    ghost_cache: crate::ghost::GhostCache,
    /// Pending cross-file sync suggestion (§4.13): kernel rename, similar
    /// lines, or hyperparameter propagation. ⌘⏎ applies it, Esc dismisses it.
    pub sync_suggestion: Option<crate::sync::SyncSuggestion>,
    /// The directory the pending suggestion's cross-file scan covered: the
    /// directory selected in the file tree at detection time, else the
    /// workspace root. Apply stays inside this scope.
    pub sync_scope: PathBuf,

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

    /// Find / replace bar state (`Some` == the ⌘F/Ctrl+F bar is open).
    pub find: Option<crate::find::FindState>,
    /// Focus handle for the find bar's captured-keystroke buffer (lazily created;
    /// None headless).
    pub find_focus: Option<FocusHandle>,
    /// Set when the find bar opens; the next render hands it focus once (so a
    /// later click into the editor isn't fought back by an every-frame re-focus).
    pub pending_find_focus: bool,
    /// Whether the find bar currently owns window focus, sampled each frame (the
    /// renderer has no `window`, so it reads this to know when to blink the field
    /// caret). False while the user has clicked into the editor to keep typing.
    pub find_bar_focused: bool,
    /// The find/replace fields' text left edges in window px (f32 bits, indexed
    /// `[Find, Replace]`), captured each frame by a canvas underlay in each
    /// field so a click can map to a char column (same pattern as
    /// [`editor_text_left`](Self::editor_text_left)).
    pub find_field_left: [Arc<AtomicU32>; 2],
    /// The mono font's char advance at the find bar's text size (f32 bits) —
    /// the bar renders at `text_xs`, not the editor's `FONT_PX`, so it needs
    /// its own measurement.
    pub find_char_w: Arc<AtomicU32>,
    /// Cached flattened file list for Quick Open, per workspace root. Rebuilt on
    /// first ⌘P and invalidated by `TreeChanged` (§5.7 "cached file list").
    file_cache: Option<Vec<FileEntry>>,

    /// Rows×cols shape-hint editor session (`Some` == open), shared by the
    /// wg3d toolbar and the telemetry sidebar's inline row editor — only one
    /// can be open at a time (weight-grid-3d.ts / telemetry-panel.ts's shape
    /// editors, §7.2 / §5.6).
    pub dim_edit: Option<dim_input::DimEditState>,
    /// Focus handle for whichever surface's dim editor is open (created lazily).
    dim_edit_focus: Option<FocusHandle>,

    // ── Build/run/debug lifecycle state ───────────────────────────────────────
    pub active_file: Option<PathBuf>,
    pub building: bool,
    pub running: bool,
    pub debugging: bool,
    pub last_build: Option<BuildResult>,
    last_sanitize: bool,
    last_instrument: bool,
    pub last_run: Option<RunStatus>,
    /// Live corner toasts (build result pop-ups), newest last. Self-expiring.
    pub toasts: Vec<Toast>,
    /// True while a toast-sweeper task is in flight, so `render` spawns at most
    /// one (mirrors `blink_task_running`).
    toast_sweeping: bool,

    // ── Runtime panel state (§5.4) ────────────────────────────────────────────
    /// Whether the RUNTIME panel is shown (toggled by the Runtime chip).
    pub runtime_visible: bool,
    /// True while the sidebar's slide-out animation plays (still rendered).
    pub sidebar_closing: bool,
    /// Bumped on every sidebar toggle so the slide animation restarts.
    pub sidebar_anim_gen: usize,
    /// True while the bottom strip's slide-out animation plays.
    pub bottom_closing: bool,
    /// Bumped on every bottom-strip toggle so its slide restarts.
    pub bottom_anim_gen: usize,
    /// Bumped on every left-sidebar collapse toggle so its slide restarts.
    pub left_anim_gen: usize,
    /// Every project opened this session (CLion-style project subtabs). The
    /// active one is `workspace_root`; switching goes through `open_project`,
    /// whose per-workspace ui persistence restores each project's tab set.
    pub open_projects: Vec<PathBuf>,
    /// Live editor state (open tabs + their buffers + scroll position) for each
    /// *inactive* project, keyed by workspace root. Stashed on switch-away so
    /// switching back keeps unsaved edits and page position in memory instead of
    /// reloading files from disk. The active project's editor lives in
    /// [`editor`](Self::editor).
    project_editors: HashMap<PathBuf, StashedProject>,
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
    pub output_scroll: gpui::ScrollHandle,
    /// Sticky-bottom: while true the OUTPUT view follows appended lines; a
    /// wheel-scroll away from the bottom releases it, scrolling back re-arms it.
    pub output_stick: bool,
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

        let mut editor = EditorState::new(TokenPalette::jade_dark());
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

        // Global AI prefs (model tier + multi-line) restored across launches.
        let ai_prefs = AiPrefs::load();
        // Apply the restored tier to the backend so the first `start()` spawns the
        // right model. `set_model` records the tier when nothing is running yet
        // (and no-ops for the default Fast), so this is safe before AI is enabled.
        {
            let ai = deps.ai.clone();
            let model = ai_prefs.model;
            deps.runtime.spawn(async move {
                ai.set_model(model).await;
            });
        }

        // Per-workspace run DB; a failed open degrades to "no run history".
        let run_store = RunStore::open(&deps.workspace_root)
            .map_err(|e| eprintln!("[jade] run store unavailable: {e}"))
            .ok();

        let mut app = Self {
            server: deps.server,
            registry: TelemetryRegistry::new(),
            training: TrainingData::new(),
            run_store,
            stored_runs: Vec::new(),
            run_overlays: Vec::new(),
            pending_run: None,
            pre_run: None,
            discovery_active: false,
            discovery_started: None,
            discovery_warm_at: None,
            discovery_decls: false,
            pending_launch: None,
            pre_run_focus: None,
            timer_groups: crate::timer_groups::GroupAggregator::new(ui.timer_groups.clone()),
            group_staging: Vec::new(),
            group_name_input: String::new(),
            buffer_search: String::new(),
            prefs: match &deps.prefs_path {
                Some(p) => TelemetryPrefs::load_from(p),
                None => TelemetryPrefs::load(),
            },
            theme: Theme::jade_dark(),
            wg3d: WeightGrid3D::new(),
            #[cfg(target_os = "macos")]
            wg3d_gpu: None,
            #[cfg(target_os = "macos")]
            wg3d_gpu_failed: false,
            preview_images: HashMap::new(),
            metric_popouts: HashMap::new(),

            engine: deps.engine,
            ai: deps.ai,
            sysmon: deps.sysmon,
            runtime: deps.runtime,
            app_tx: deps.app_tx,
            repo_root: deps.repo_root,
            workspace_root: deps.workspace_root.clone(),
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
            term_scroll_back: 0,
            term_sel: None,
            term_sel_dragging: false,
            term_origin: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            bottom_height: 220.,
            bottom_resize: None,

            tree,
            tree_selection: None,
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
            signature: None,
            signature_gen: 0,
            sig_help_requests: 0,

            ai_completion_enabled: ui.ai_completion_enabled.unwrap_or(true),
            ai_multiline: ai_prefs.multiline,
            ai_model: ai_prefs.model,
            ai_menu_open: false,
            ai_prefs,
            ghost: None,
            ghost_gen: 0,
            ghost_cache: crate::ghost::GhostCache::new(),
            sync_suggestion: None,
            sync_scope: deps.workspace_root.clone(),

            xp: crate::xp::XpState::new(xp_total),
            xp_store,

            sidebar_tab: SidebarTab::Files,
            sidebar_collapsed: false,
            quick_open: None,
            quick_open_focus: None,
            find: None,
            find_field_left: [
                Arc::new(AtomicU32::new(0)),
                Arc::new(AtomicU32::new(0)),
            ],
            find_char_w: Arc::new(AtomicU32::new(0)),
            find_focus: None,
            pending_find_focus: false,
            find_bar_focused: false,
            file_cache: None,
            dim_edit: None,
            dim_edit_focus: None,

            active_file,
            building: false,
            running: false,
            debugging: false,
            last_build: None,
            last_sanitize: false,
            last_instrument: false,
            last_run: None,
            toasts: Vec::new(),
            toast_sweeping: false,

            runtime_visible: false,
            sidebar_closing: false,
            sidebar_anim_gen: 0,
            bottom_closing: false,
            bottom_anim_gen: 0,
            left_anim_gen: 0,
            open_projects: if opened {
                vec![deps.workspace_root]
            } else {
                Vec::new()
            },
            project_editors: HashMap::new(),
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
            output_scroll: gpui::ScrollHandle::new(),
            output_stick: true,
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
        };
        // Bring the completion backend up at launch when the toggle is on
        // (§4.11). Without this, every fresh session started with the switch
        // "on" but the server Disabled — ghost text stayed dead until the
        // sparkle menu happened to be opened.
        app.ensure_ai_started();
        app.refresh_stored_runs();
        app.declare_loaded_timer_groups();
        app.seed_registry_from_prefs();
        app
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
            AppEvent::Ai(status) => {
                // On the transition *to* Ready, offer a ghost at the caret right
                // away — otherwise the first suggestion after startup waits for
                // one more keystroke that may never come.
                let was_ready = self.ai_status.state == AiState::Ready;
                self.ai_status = status;
                if !was_ready && self.ai_status.state == AiState::Ready {
                    self.schedule_ghost();
                }
            }
            AppEvent::Term(ev) => self.on_term_event(ev),
            AppEvent::DiscoveryTick => {
                if self.discovery_active {
                    if let Some(start) = self.discovery_started {
                        let warm_for = self.discovery_warm_at.map(|w| w.elapsed());
                        if discovery_should_stop(start.elapsed(), warm_for, self.discovery_decls) {
                            self.engine.stop();
                        }
                    }
                }
            }
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
            AppEvent::SignatureHelp {
                generation,
                hint,
                anchor,
            } => self.on_signature_help(generation, hint, anchor),
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

    /// Follow the `jade-term` contract: on `Damaged` re-snapshot the current
    /// instance; on `Exited` record the code for the dim `[exited …]` line.
    fn on_term_event(&mut self, ev: TermEvent) {
        match ev {
            TermEvent::Damaged { id } => {
                if self.term_id == Some(id) {
                    let old_sb = self.term_snapshot.as_ref().map(|s| s.scrollback.len());
                    self.term_snapshot = self.term.snapshot(id);
                    // Keep a scrolled-up view anchored to its content: as lines
                    // spill into scrollback, grow the offset by the same amount
                    // so what the user is reading stays in place (clamped in the
                    // renderer). At the live bottom (offset 0) we stay pinned.
                    if self.term_scroll_back > 0 {
                        if let (Some(old), Some(new)) =
                            (old_sb, self.term_snapshot.as_ref().map(|s| s.scrollback.len()))
                        {
                            let grown = new.saturating_sub(old);
                            self.term_scroll_back =
                                (self.term_scroll_back + grown).min(new);
                        }
                    }
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

    /// Where a new terminal starts: the directory selected in the file tree (a
    /// selected file means its parent directory), else the active file's
    /// directory, else the workspace root (§5.2).
    pub fn terminal_cwd(&self) -> PathBuf {
        terminal_cwd_for(
            self.tree_selection.as_deref(),
            self.active_file.as_deref(),
            &self.workspace_root,
        )
    }

    /// Create the single terminal instance on first show (cwd = the selected
    /// directory, see [`Self::terminal_cwd`]).
    /// Degrades gracefully if the PTY can't be allocated (§5.2).
    fn ensure_terminal(&mut self) {
        if self.term_id.is_some() || self.term_failed {
            return;
        }
        let cwd = self.terminal_cwd();
        match self.term.create(&cwd) {
            Ok(id) => {
                self.term_id = Some(id);
                self.term_exited = false;
                self.term_exit_code = None;
                self.term_last_size.store(0, std::sync::atomic::Ordering::Relaxed);
                self.term_snapshot = self.term.snapshot(id);
            }
            Err(e) => {
                self.term_failed = true;
                self.status_line(&format!("[jade] terminal unavailable: {e}"));
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
        self.term_scroll_back = 0;
        self.term_last_size.store(0, std::sync::atomic::Ordering::Relaxed);
        self.bottom_view = BottomView::Terminal;
        self.output_visible = true;
        self.bottom_closing = false;
        self.ensure_terminal();
    }

    /// Toggle the RUNTIME panel (Runtime chip, §5.4).
    /// Timer/gauge button: slides the whole right sidebar (runtime graphs +
    /// training + telemetry) open/closed. All three regions coexist — the
    /// flex layout resizes around whichever are open. Closing keeps the
    /// sidebar rendered while the slide-out plays, then drops it.
    pub fn action_toggle_runtime(&mut self, cx: &mut Context<Self>) {
        self.sidebar_anim_gen += 1;
        if self.runtime_visible && !self.sidebar_closing {
            self.sidebar_closing = true;
            let gen = self.sidebar_anim_gen;
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(SIDEBAR_SLIDE_MS))
                    .await;
                let _ = this.update(cx, |app, cx| {
                    // Ignore if re-toggled mid-slide (gen moved on).
                    if app.sidebar_closing && app.sidebar_anim_gen == gen {
                        app.sidebar_closing = false;
                        app.runtime_visible = false;
                        cx.notify();
                    }
                });
            })
            .detach();
        } else {
            self.sidebar_closing = false;
            self.runtime_visible = true;
        }
    }

    /// Toggle the execution-flow decorations (Flow chip / ⌘E, §4.8).
    pub fn action_toggle_flow(&mut self) {
        self.flow_visible = !self.flow_visible;
    }

    /// Cmd+Click glyph navigation (§4.8): reveal a target line in the viewer.
    /// The flow analysis is single-file, so all targets are in the active tab.
    pub fn flow_goto(&mut self, line_1based: usize) {
        if line_1based >= 1 {
            // Unfold anything hiding the target, then scroll in display space.
            let display = match self.editor.active_tab_mut() {
                Some(tab) => {
                    tab.unfold_containing(line_1based - 1);
                    tab.display_row(line_1based - 1)
                }
                None => line_1based - 1,
            };
            self.code_scroll
                .scroll_to_item(display, gpui::ScrollStrategy::Center);
        }
    }

    /// Switch the bottom panel between the live TERMINAL and the OUTPUT
    /// scrollback fallback.
    pub fn set_bottom_view(&mut self, view: BottomView) {
        self.bottom_view = view;
        self.output_visible = true;
        self.bottom_closing = false;
    }

    fn status_line(&mut self, s: &str) {
        push_output(&mut self.output, s);
    }

    /// Raise a transient corner toast. Caps the stack at 3 (drops the oldest) so
    /// a burst of rebuilds can't wallpaper the editor. The sweeper that expires
    /// it is (re)spawned lazily in `render` — no cx needed here, so this stays
    /// callable from the cx-less `apply_app_event` choke point.
    pub fn push_toast(&mut self, kind: ToastKind, message: impl Into<String>) {
        let created_ms = self.now_ms();
        self.toasts.push(Toast {
            message: message.into(),
            kind,
            created_ms,
        });
        let overflow = self.toasts.len().saturating_sub(3);
        if overflow > 0 {
            self.toasts.drain(0..overflow);
        }
    }

    fn on_build_done(&mut self, res: BuildResult) {
        self.building = false;
        let ms = res.duration.as_millis();
        if res.success {
            self.status_line(&format!("[jade] Build succeeded ({ms}ms)"));
            self.push_toast(ToastKind::Success, format!("Build succeeded · {ms}ms"));
        } else {
            // Count only real errors — clang's warnings ride along in `errors`
            // with their own severity, and leading with them buries the failure.
            let nerr = res
                .errors
                .iter()
                .filter(|e| e.severity == jade_build::Severity::Error)
                .count();
            self.status_line(&format!("[jade] Build failed ({nerr} error(s), {ms}ms)"));
            let plural = if nerr == 1 { "error" } else { "errors" };
            self.push_toast(
                ToastKind::Error,
                format!("Build failed · {nerr} {plural}"),
            );
            for e in &res.errors {
                let tag = match e.severity {
                    jade_build::Severity::Error => "error",
                    jade_build::Severity::Warning => "warning",
                    jade_build::Severity::Note => "note",
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

    /// Fold one `__JADE_*` instrumentation line from the DEBUG console into
    /// app state — the LLDB-path mirror of `run.rs`'s `handle_stdout`/
    /// `handle_stderr`: alloc/free + heap summaries feed the memory bar and
    /// Memory chart; scalar/timing macro lines feed the telemetry server
    /// (probe traffic arrives over the socket and never comes through here).
    fn apply_jade_line(&mut self, line: &str) {
        if let Some(ev) = parse_alloc_free(line) {
            let batch = MemoryEvent::AllocBatch { events: vec![ev] };
            if let Some(sample) = self.mem.apply(&batch) {
                self.training.push_memory(sample);
            }
        } else if let Some(ev) = parse_heap_summary(line) {
            if let Some(sample) = self.mem.apply(&ev) {
                self.training.push_memory(sample);
            }
        } else if let Some(s) = parse_scalar(line) {
            self.server.ingest_scalar(s);
        } else if let Some(t) = parse_timing(line) {
            self.server.ingest_timing(t);
        }
        // Anything else (`__JADE_INTERPOSE_ACTIVE`, unrecognized lines) is
        // swallowed, matching the Run path.
    }

    /// Open one TRAINING section (Loss / Memory / Kernel time) in its own
    /// resizable window — big charts for screen recording (§7.1 "⧉" buttons).
    /// If the section's window is already open it's re-focused instead.
    ///
    /// The open is deferred: `open_window` draws the new window synchronously,
    /// and its root view reads THIS entity — which is still leased out to the
    /// click listener we're called from. `App::defer` runs after the lease
    /// returns.
    pub fn open_metric_popout(&mut self, section: MetricSection, cx: &mut Context<Self>) {
        if let Some(handle) = self.metric_popouts.get(&section).copied() {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
        }
        let entity = cx.entity();
        cx.defer(move |cx| {
            let bounds = Bounds::centered(None, gpui::size(px(1100.), px(720.)), cx);
            let opened = cx.open_window(
                gpui::WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some(format!("Jade — {}", section.title()).into()),
                        appears_transparent: false,
                        traffic_light_position: None,
                    }),
                    window_min_size: Some(gpui::size(px(480.), px(320.))),
                    window_background: gpui::WindowBackgroundAppearance::Opaque,
                    ..Default::default()
                },
                {
                    let entity = entity.clone();
                    move |_, cx| cx.new(|cx| MetricPopout::new(entity, section, cx))
                },
            );
            if let Ok(handle) = opened {
                entity.update(cx, |app, _| {
                    app.metric_popouts.insert(section, handle);
                });
            }
        });
    }

    fn on_run_done(&mut self, res: RunResult) {
        // Discovery scans are not runs: no run-store record, no HISTORY entry,
        // no best-time stats. Drop the scan's telemetry junk and, if the user
        // hit Run mid-scan, dispatch the deferred launch now.
        if self.discovery_active {
            self.discovery_active = false;
            self.discovery_started = None;
            self.discovery_warm_at = None;
            self.discovery_decls = false;
            if let Some(p) = self.pre_run.as_mut() {
                p.discovering = false;
            }
            self.training.current = Default::default();
            self.training.tensors.clear();
            self.mem.reset();
            self.prune_stale_seeded();
            let timers = self.registry.items_of_kind(Kind::Timer).len();
            let buffers = self.registry.items_of_kind(Kind::Buffer).len();
            self.status_line(&format!(
                "[jade] Discovery done — {timers} timers, {buffers} buffers"
            ));
            if let Some(launch) = self.pending_launch.take() {
                self.dispatch_launch(launch);
            }
            return;
        }

        self.running = false;
        self.run_started = None;
        self.prune_stale_seeded();
        let ms = res.duration.as_millis();
        let executed = res.executed_lines.len();
        // Persist the finished run's telemetry to the run store (§7 research
        // studio). `training.current` still holds this run — the ghost-snapshot
        // clear happens at the *next* run's start.
        self.persist_finished_run(Some(ms as i64), Some(res.exit_code as i64));

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
            self.status_line(&format!("[jade] Exited with code 0 ({ms}ms)"));
        } else {
            self.status_line(&format!(
                "[jade] Exited with code {} ({ms}ms)",
                res.exit_code
            ));
        }
        if res.interpose_active {
            self.status_line("[jade] Memory tracked via malloc interposer");
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
            DebugEvent::Output(s) => {
                // `__JADE_*` lines the console swallowed are instrumentation
                // data (see `DebugState::push_console`) — route them the same
                // way the Run path's stdio handlers do.
                for line in self.debug.push_console(&s) {
                    self.apply_jade_line(&line);
                }
            }
            DebugEvent::Stopped {
                reason,
                file,
                line,
                frames,
                locals,
            } => {
                let refetch =
                    self.debug.on_stopped(reason.clone(), file.clone(), line, frames, locals);
                self.status_line(&format!("[jade] paused at {file}:{line} ({reason})"));
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
                // Wall-clock duration from the launch context (coarse — epoch
                // seconds — but debug sessions include human-paced stepping, so
                // sub-second precision isn't meaningful anyway).
                let dur = self
                    .pending_run
                    .as_ref()
                    .map(|p| (crate::run_store::now_epoch() - p.started_epoch).max(0) * 1000);
                self.persist_finished_run(dur, Some(code as i64));
                self.status_line(&format!("[jade] debug exited ({code})"));
            }
        }
    }

    // ── Run store (research-studio run history) ──────────────────────────────

    /// Re-read the run list from the store (after save/delete/workspace switch).
    fn refresh_stored_runs(&mut self) {
        self.stored_runs = self
            .run_store
            .as_ref()
            .and_then(|s| s.list_runs().ok())
            .unwrap_or_default();
    }

    /// Write the just-finished run (`training.current` + the launch context
    /// captured at start) to the store. No-ops without a pending run or store;
    /// empty runs (no telemetry) are skipped by the store itself.
    pub(crate) fn persist_finished_run(&mut self, duration_ms: Option<i64>, exit_code: Option<i64>) {
        let Some(pending) = self.pending_run.take() else {
            return;
        };
        let Some(store) = self.run_store.as_mut() else {
            return;
        };
        match store.save_run(&pending, &self.training.current, duration_ms, exit_code) {
            Ok(Some(_)) => self.refresh_stored_runs(),
            Ok(None) => {} // no telemetry — not a research run
            Err(e) => self.status_line(&format!("[jade] run store write failed: {e}")),
        }
    }

    /// Toggle a stored run as a chart overlay (loads its series on first
    /// toggle; unloading just drops the in-memory copy).
    pub fn toggle_run_overlay(&mut self, id: i64) {
        if let Some(i) = self.run_overlays.iter().position(|(rid, _)| *rid == id) {
            self.run_overlays.remove(i);
            return;
        }
        let Some(store) = self.run_store.as_ref() else {
            return;
        };
        match store.load_run(id) {
            Ok(Some(data)) => self.run_overlays.push((id, data)),
            Ok(None) => self.refresh_stored_runs(), // stale row — deleted elsewhere
            Err(e) => self.status_line(&format!("[jade] run load failed: {e}")),
        }
    }

    /// Delete a stored run from the DB, the overlay set, and the cached list.
    pub fn delete_stored_run(&mut self, id: i64) {
        if let Some(store) = self.run_store.as_ref() {
            if let Err(e) = store.delete_run(id) {
                self.status_line(&format!("[jade] run delete failed: {e}"));
            }
        }
        self.run_overlays.retain(|(rid, _)| *rid != id);
        self.refresh_stored_runs();
    }

    // ── Pre-run tracking panel (discover → select → launch) ──────────────────

    /// Open the panel. If the panel would be completely empty (no probe decls
    /// AND no seeded items — i.e. this project has never run) and a built
    /// executable exists, a discovery run starts automatically to fill it.
    /// A seeded list (persisted selections, loaded bundle defs) is shown as-is
    /// even though it may be stale: auto-scanning would launch the user's
    /// program while they're still mid-selection, which reads as "Run started
    /// my program before I confirmed". The header's Rescan button covers
    /// refreshing names after a refactor.
    pub fn open_pre_run(&mut self, launch: PreRunLaunch) {
        let have_items = !self.registry.items_of_kind(Kind::Timer).is_empty()
            || !self.registry.items_of_kind(Kind::Buffer).is_empty();
        self.pre_run = Some(PreRunPanel { launch, discovering: false });
        // Fresh editing state per open (stale staging/filters confuse).
        self.group_staging.clear();
        self.group_name_input.clear();
        self.buffer_search.clear();
        if !have_items && self.can_run() {
            self.start_discovery();
        }
    }

    /// Launch the built executable briefly (killed after [`DISCOVERY_SECS`])
    /// with everything untracked, so the probe's decls fill the registry with
    /// the program's timer/buffer names for the panel to list.
    pub fn start_discovery(&mut self) {
        if self.running || self.discovery_active || !self.can_run() {
            return;
        }
        let exe = self
            .last_build
            .as_ref()
            .and_then(|b| b.executable.clone())
            .expect("can_run checked");
        self.discovery_active = true;
        self.discovery_started = Some(std::time::Instant::now());
        self.discovery_warm_at = None;
        self.discovery_decls = false;
        if let Some(p) = self.pre_run.as_mut() {
            p.discovering = true;
        }
        // The scan's junk telemetry lands in training.current and is dropped
        // again at scan end.
        self.training.clear();
        self.status_line("[jade] Discovering telemetry…");

        let cfg = RunConfig {
            executable: exe,
            args: Vec::new(),
            enable_sanitizers: false,
            enable_instrumentation: false,
            cwd: self
                .last_build
                .as_ref()
                .and_then(|b| b.project_root.clone())
                .or_else(|| Some(self.workspace_root.clone())),
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
        // Watchdog: tick until the handler decides the scan is done (warm app
        // + post-warm window, or a cap — see `discovery_should_stop`). Ticks
        // after the child exits are ignored (`discovery_active` false), and the
        // loop is bounded past the hard cap so the task always ends.
        let tx = self.app_tx.clone();
        self.runtime.spawn(async move {
            for _ in 0..(2 * (DISCOVERY_HARD_CAP_SECS + DISCOVERY_SECS) + 4) {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if tx.send(AppEvent::DiscoveryTick).is_err() {
                    break;
                }
            }
        });
    }

    /// Panel Run button / Enter: close the panel and launch. If the discovery
    /// child is still up, defer the launch until its `RunDone` lands (the run
    /// engine allows one child at a time).
    pub fn confirm_pre_run(&mut self) {
        let Some(panel) = self.pre_run.take() else {
            return;
        };
        if self.discovery_active {
            self.pending_launch = Some(panel.launch);
            self.engine.stop();
            return;
        }
        self.dispatch_launch(panel.launch);
    }

    /// Panel Cancel / Esc: close without launching; stop any discovery child.
    pub fn cancel_pre_run(&mut self) {
        self.pre_run = None;
        self.pending_launch = None;
        if self.discovery_active {
            self.engine.stop();
        }
    }

    fn dispatch_launch(&mut self, launch: PreRunLaunch) {
        match launch {
            PreRunLaunch::Run => self.launch_run(),
            PreRunLaunch::Debug => self.launch_debug(),
        }
    }

    /// Stage/unstage a timer for the next bundle (panel "◧" toggle).
    pub fn toggle_group_staging(&mut self, name: &str) {
        if let Some(i) = self.group_staging.iter().position(|n| n == name) {
            self.group_staging.remove(i);
            if self.group_staging.is_empty() {
                self.group_name_input.clear();
            }
        } else {
            self.group_staging.push(name.to_string());
        }
    }

    /// Keystroke routing for the panel's two captured-keystroke buffers.
    /// While the bundle-staging editor is active (≥1 timer staged) printable
    /// chars build the group name, Enter creates the bundle, Esc clears the
    /// staging. Otherwise printable chars edit the buffer filter and Esc
    /// clears it. Returns whether the key was consumed (false → the panel's
    /// normal Esc/Enter behavior applies).
    pub fn pre_run_key(&mut self, key: &str, key_char: Option<&str>, printable: bool) -> bool {
        if self.group_staging.is_empty() {
            // Buffer filter editing.
            match key {
                "backspace" if !self.buffer_search.is_empty() => {
                    self.buffer_search.pop();
                    return true;
                }
                "escape" if !self.buffer_search.is_empty() => {
                    self.buffer_search.clear();
                    return true;
                }
                _ if printable => {
                    if let Some(c) = key_char {
                        self.buffer_search.push_str(c);
                        return true;
                    }
                    return false;
                }
                _ => return false,
            }
        }
        match key {
            "escape" => {
                self.group_staging.clear();
                self.group_name_input.clear();
            }
            "enter" => {
                if self.group_name_input.trim().is_empty() {
                    return true; // a bundle needs a name — swallow, keep staging
                }
                let members = std::mem::take(&mut self.group_staging);
                let name = std::mem::take(&mut self.group_name_input);
                self.create_timer_group(&name, members);
            }
            "backspace" => {
                self.group_name_input.pop();
            }
            _ if printable => {
                if let Some(c) = key_char {
                    self.group_name_input.push_str(c);
                }
            }
            _ => return false,
        }
        true
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
            self.status_line("[jade] No active file — pass --file or --project");
            return false;
        };
        if self.building {
            return false;
        }
        self.building = true;
        self.mem.reset(); // resetMemoryTracking at build time (app.ts:1024)
        self.output_visible = true;
        self.bottom_closing = false; // cancel a mid-slide close
        self.bottom_view = BottomView::Output; // build progress/errors live here
        self.last_sanitize = sanitize;
        self.last_instrument = instrument;
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.status_line(&format!("[jade] Building {name}..."));

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

    /// Run button / ⌘R: opens the pre-run tracking panel (pick timers/buffers
    /// first); the panel's Run confirms into [`launch_run`](Self::launch_run).
    pub fn action_run(&mut self) {
        if self.last_build.is_none() {
            self.status_line("[jade] Build first");
            return;
        }
        if self.running || self.discovery_active {
            return;
        }
        self.open_pre_run(PreRunLaunch::Run);
    }

    /// Launch the last successful build (deliverable §3) — the pre-run panel's
    /// confirm path. Enabled only after a successful build with an executable.
    pub fn launch_run(&mut self) {
        let Some(build) = self.last_build.as_ref() else {
            self.status_line("[jade] Build first");
            return;
        };
        if !build.success {
            self.status_line("[jade] Last build failed — nothing to run");
            return;
        }
        let Some(exe) = build.executable.clone() else {
            self.status_line("[jade] Build produced no executable");
            return;
        };
        if self.running {
            return;
        }
        self.running = true;
        self.run_started = Some(Instant::now()); // drive the live SPEED tick
        self.error_line = None; // clear any prior run's error line
        self.training.clear(); // fresh charts per run (compare via RUNS overlays)
        // (preview_images prunes on the next render — dropping textures needs
        // the window, see ensure_preview_images.)
        self.mem.reset(); // reset run-memory state
        self.output.clear(); // fresh OUTPUT scrollback per run
        self.output_stick = true; // a new run always starts following the tail
        self.output_visible = true;
        self.bottom_closing = false;
        self.bottom_view = BottomView::Output; // run output/exit status lands here
        let name = exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Launch context for the run store; consumed by `persist_finished_run`
        // in `on_run_done` (captures start time + git sha *before* the run).
        self.pending_run = Some(PendingRun::begin(name.clone(), KIND_RUN, &self.workspace_root));
        self.status_line(&format!("[jade] Running ./{name}..."));

        let cfg = RunConfig {
            executable: exe,
            args: Vec::new(),
            enable_sanitizers: self.last_sanitize,
            enable_instrumentation: self.last_instrument,
            // Run from the BUILT project's root (like CLion) so the program
            // finds its data files by relative path — not from the
            // cmake-build-* dir, and not from the active workspace, which can
            // be a different project than the one that was built.
            cwd: self
                .last_build
                .as_ref()
                .and_then(|b| b.project_root.clone())
                .or_else(|| Some(self.workspace_root.clone())),
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
    /// Debug button: opens the pre-run tracking panel first (same flow as Run);
    /// confirming lands in [`launch_debug`](Self::launch_debug).
    pub fn action_debug(&mut self) {
        if self.active_file.is_none() {
            self.status_line("[jade] No active file — pass --file or --project");
            return;
        }
        if self.building || self.debugging || self.discovery_active {
            return;
        }
        self.open_pre_run(PreRunLaunch::Debug);
    }

    pub fn launch_debug(&mut self) {
        let Some(file) = self.active_file.clone() else {
            self.status_line("[jade] No active file — pass --file or --project");
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
        // Each debug session is its own run: reset the charts (the Run path
        // does this in `launch_run`) and capture the launch context for the
        // run store.
        self.training.clear();
        let label = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "debug".to_string());
        self.pending_run = Some(PendingRun::begin(label, KIND_DEBUG, &self.workspace_root));
        self.status_line("[jade] Debug build (forced -O0)...");
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
        let server = self.server.clone();
        let sock = self.server.socket_path().display().to_string();
        let root = self.workspace_root.clone();
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
            let proj_root = res.project_root.clone();
            let _ = tx.send(AppEvent::BuildDone(res));
            if let (true, Some(exe)) = (success, exe) {
                // Buffer-name aliasing needs the atos symbolicator, and this
                // path launches via LLDB — not engine.run(), which is where it
                // is normally installed. Without this, buffers discovered while
                // debugging keep their raw `Class::Class #N` probe names.
                server.set_symbolicator(Arc::new(AtosSymbolicator::new(exe.clone())));
                // env seam: telemetry socket always; dylibs only if they built.
                // The malloc interposer rides along like the Run path injects
                // it (debug builds never sanitize, so no ASan conflict) — its
                // periodic heap summaries come back through the LLDB console
                // and feed the Memory chart via `apply_jade_line`.
                let mut env = vec![("JADE_TELEMETRY_SOCK".to_string(), sock)];
                let mut dylibs: Vec<&str> = Vec::new();
                if engine.ensure_interpose_dylib() {
                    dylibs.push(INTERPOSE_DYLIB);
                }
                if engine.ensure_probe_dylib() {
                    dylibs.push(PROBE_DYLIB);
                }
                if !dylibs.is_empty() {
                    env.push(("DYLD_INSERT_LIBRARIES".to_string(), dylibs.join(":")));
                }
                // Debug from the BUILT project's root (like Run / CLion) so the
                // program finds its data files by relative path — not the build
                // dir, and not a different active workspace.
                let cwd = proj_root.unwrap_or(root).display().to_string();
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
        self.status_line("[jade] Stopped");
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

    /// Hand the active file off to CLion at the caret line (⌘⇧C / the CLion
    /// chip). Jade is the research studio, CLion the editor — this is the seam.
    /// Launcher candidates, in order: `JADE_CLION` override, the Toolbox CLI
    /// script, the app-bundle binary (both forward to a running instance), and
    /// finally a bare `open -a CLion` (no line, but never fails silently).
    pub fn action_open_in_clion(&mut self) {
        let Some(tab) = self.editor.active_tab() else {
            return;
        };
        let path = tab.path.clone();
        let line = tab.caret_point().row + 1;
        let display = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.status_line(&format!("[jade] Opening in CLion: {display}:{line}"));

        let file = path.display().to_string();
        self.runtime.spawn(async move {
            let mut candidates: Vec<(String, Vec<String>)> = Vec::new();
            if let Ok(bin) = std::env::var("JADE_CLION") {
                if !bin.is_empty() {
                    candidates.push((bin, vec!["--line".into(), line.to_string(), file.clone()]));
                }
            }
            if let Some(home) = std::env::var_os("HOME") {
                let toolbox = std::path::Path::new(&home)
                    .join("Library/Application Support/JetBrains/Toolbox/scripts/clion");
                if toolbox.exists() {
                    candidates.push((
                        toolbox.display().to_string(),
                        vec!["--line".into(), line.to_string(), file.clone()],
                    ));
                }
            }
            let bundle = "/Applications/CLion.app/Contents/MacOS/clion";
            if std::path::Path::new(bundle).exists() {
                candidates.push((
                    bundle.to_string(),
                    vec!["--line".into(), line.to_string(), file.clone()],
                ));
            }
            candidates.push((
                "/usr/bin/open".to_string(),
                vec!["-a".into(), "CLion".into(), file.clone()],
            ));

            for (bin, args) in candidates {
                // tokio Command so the child is reaped on exit (a dropped std
                // Child would linger as a zombie once the launcher returns).
                let spawned = tokio::process::Command::new(&bin)
                    .args(&args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                if spawned.is_ok() {
                    return;
                }
            }
        });
    }

    /// Swap jade-dark / jade-light (deliverable §3). Also swaps the editor's
    /// token palette so syntax highlighting re-resolves for the new theme (§4.2).
    pub fn action_theme(&mut self) {
        let light = !self.theme.is_light;
        self.theme = if light {
            Theme::jade_light()
        } else {
            Theme::jade_dark()
        };
        self.editor.set_palette(if light {
            TokenPalette::jade_light()
        } else {
            TokenPalette::jade_dark()
        });
    }

    /// The syntax-highlight palette matching the active theme.
    fn editor_palette(&self) -> TokenPalette {
        if self.theme.is_light {
            TokenPalette::jade_light()
        } else {
            TokenPalette::jade_dark()
        }
    }

    /// Toggle the output panel visibility.
    pub fn action_toggle_output(&mut self, cx: &mut Context<Self>) {
        self.bottom_anim_gen += 1;
        if self.output_visible && !self.bottom_closing {
            self.bottom_closing = true;
            let gen = self.bottom_anim_gen;
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(SIDEBAR_SLIDE_MS))
                    .await;
                let _ = this.update(cx, |app, cx| {
                    if app.bottom_closing && app.bottom_anim_gen == gen {
                        app.bottom_closing = false;
                        app.output_visible = false;
                        cx.notify();
                    }
                });
            })
            .detach();
        } else {
            self.bottom_closing = false;
            self.output_visible = true;
        }
    }

    // ── Code-viewing vertical (file tree + tabs + viewer) ─────────────────────

    /// Open a file in the editor (deliverable §3): reads + highlights it once
    /// (deduped by path), makes it the active tab, and points `active_file` at it
    /// so the Build/Run target follows the front tab.
    pub fn open_file(&mut self, path: PathBuf) {
        // Remember the outgoing tab's page position before we switch/open, then
        // restore the destination tab's own remembered position.
        self.stash_scroll();
        match self.editor.open(&path) {
            Ok(_) => {
                self.active_file = self.editor.active_path();
                self.after_open_active(&path);
                self.apply_scroll();
                self.find_resync();
            }
            Err(e) => self.status_line(&format!("[jade] Could not open {}: {e}", path.display())),
        }
    }

    /// Shared post-`editor.open` bookkeeping for the active file (LSP init +
    /// `didOpen`, focus, popup dismissal) — no scroll handling, so the project
    /// switch path can manage the page position itself.
    fn after_open_active(&mut self, path: &Path) {
        self.dismiss_popups();
        self.pending_editor_focus = true; // caret + keys live immediately
        self.ensure_lsp();
        self.lsp_did_open(path);
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
        if self.workspace_root == dir && self.workspace_opened {
            return; // already the active project
        }
        // Persist the outgoing workspace's tab set so switching back restores it.
        if self.workspace_opened {
            self.save_ui_state();
            // Snapshot the active tab's page position, then stash the outgoing
            // project's live editor — open tabs and their buffers (unsaved edits
            // included) plus each tab's remembered scroll — so switching back
            // restores the in-memory state instead of reloading pristine files.
            self.stash_scroll();
            let outgoing = std::mem::take(&mut self.editor);
            self.project_editors
                .insert(self.workspace_root.clone(), StashedProject { editor: outgoing });
        }
        // Register in the project subtabs (dedupe; active follows workspace_root).
        if !self.open_projects.iter().any(|p| p == &dir) {
            self.open_projects.push(dir.clone());
        }
        // Leaving any prior workspace: drop its clangd session so the new root
        // re-initializes lazily on the first file open (clangd is per-workspace).
        self.lsp = None;
        self.lsp_init_started = false;

        self.workspace_root = dir.clone();
        self.workspace_opened = true;
        self.tree = Some(FileTree::scan(dir.clone()));
        self.tree_selection = None; // the old selection is outside the new root
        self.sync_suggestion = None; // a pending suggestion refers to the old project
        self.sync_scope = dir.clone();
        self.file_cache = None; // Quick Open cache is per-workspace (§5.7)

        // The run DB is per-workspace: swap stores and drop overlays/pending
        // context that referred to the old one.
        self.run_store = RunStore::open(&dir)
            .map_err(|e| eprintln!("[jade] run store unavailable: {e}"))
            .ok();
        self.run_overlays.clear();
        self.pending_run = None;
        self.refresh_stored_runs();

        // Restore this folder's persisted `ui` blob (§1.2), same fields startup
        // reads: breakpoints / benchmarks / ai toggle / terminal visibility.
        let ui = crate::workspace_state::load(&dir);
        self.breakpoints = crate::debug::Breakpoints::from_map(ui.breakpoints.clone());
        self.benchmarks = ui.benchmarks.clone();
        // Timer bundles are per-workspace (kernel names differ per project).
        self.timer_groups =
            crate::timer_groups::GroupAggregator::new(ui.timer_groups.clone());
        self.declare_loaded_timer_groups();
        self.group_staging.clear();
        self.group_name_input.clear();
        if let Some(v) = ui.ai_completion_enabled {
            self.ai_completion_enabled = v;
        }
        // The new workspace may enable completion while the backend is idle
        // (e.g. the previous folder had it off) — same launch rule as assemble.
        self.ensure_ai_started();
        self.output_visible = ui.terminal_visible.unwrap_or(true);

        // Prefer the in-memory editor if we visited this project earlier this
        // session (keeps unsaved edits); otherwise build a fresh one from the
        // persisted tab set on disk.
        if let Some(saved) = self.project_editors.remove(&dir) {
            let StashedProject { mut editor } = saved;
            // The old clangd session is gone; force each tab to re-`didOpen` in
            // the new root's session when it's next made active.
            for tab in &mut editor.tabs {
                tab.lsp_opened = false;
            }
            self.editor = editor;
            // Re-resolve syntax colors in case the theme changed while away.
            self.editor.set_palette(self.editor_palette());
        } else {
            self.editor = EditorState::new(self.editor_palette());
            // Restore persisted open tabs (skipping deleted files), then the
            // active index — else fall back to the folder's first source file so
            // the editor isn't blank (mirrors --project's first-file pick).
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
        }
        self.active_file = self.editor.active_path();

        // Route the front tab through the real open path (LSP init + didOpen +
        // highlight + editor focus). We manage the scroll ourselves (below) rather
        // than through `open_file`, whose stash-then-read would clobber the
        // restored tab's remembered position with the stale, pre-restore offset.
        if let Some(path) = self.active_file.clone() {
            let _ = self.editor.open(&path);
            self.after_open_active(&path);
        }
        // Restore the active tab's remembered page position (deferred to next paint).
        self.apply_scroll();

        // Restart the fs-watch on the new root (§5.1).
        self.restart_fs_watch();

        // Status line reflects the opened folder (the build/run target follows
        // `active_file`, so both work against the new workspace immediately).
        self.status_line(&format!("[jade] Opened {}", dir.display()));
    }

    /// Close an open project subtab (§2). Removes `dir` from `open_projects`;
    /// closing the *active* project switches to a neighbor (the one that slid into
    /// its slot, else the new last) via [`open_project`], so the editor/tree never
    /// go blank. The last remaining project can't be closed (nothing to switch
    /// to) — the subtab UI only offers a close affordance once ≥2 are open, so
    /// closing always leaves ≥1. `cx`-free so the transition is unit-testable.
    pub fn close_project(&mut self, dir: &Path) {
        let Some(idx) = self.open_projects.iter().position(|p| p == dir) else {
            return;
        };
        // Never close the only project — there'd be nowhere to switch to.
        if self.open_projects.len() < 2 {
            return;
        }
        let was_active = *dir == self.workspace_root;
        self.open_projects.remove(idx);
        // Closing a project discards its stashed editor (its unsaved edits go with
        // it — the tab strip offers no save prompt, matching close-tab semantics).
        self.project_editors.remove(dir);
        if was_active {
            // Prefer the project now occupying `idx` (was the next one along),
            // else the new last — either way a different root, so `open_project`
            // won't early-return.
            let next = self
                .open_projects
                .get(idx)
                .or_else(|| self.open_projects.last())
                .cloned();
            if let Some(next) = next {
                self.open_project(next);
            }
        } else {
            self.status_line(&format!("[jade] Closed {}", dir.display()));
        }
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

    /// Mark a file-tree row as the selection. The panel highlights it, and the
    /// next terminal opens there (§5.2).
    pub fn select_tree_path(&mut self, path: PathBuf) {
        self.tree_selection = Some(path);
    }

    /// Switch the active tab; `active_file` follows. The outgoing tab's page
    /// position is stashed and the incoming tab's is restored, so each tab keeps
    /// its own scroll.
    pub fn switch_tab(&mut self, index: usize) {
        if self.editor.active == Some(index) {
            return; // already active — don't disturb the scroll
        }
        self.stash_scroll();
        self.editor.switch(index);
        self.active_file = self.editor.active_path();
        self.apply_scroll();
        self.find_resync();
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
        let prev_active = self.active_file.clone();
        self.editor.close(index);
        self.active_file = self.editor.active_path();
        // Only restore scroll when the active *tab* actually changed (closing a
        // background tab must not jerk the current tab to a stale position).
        if self.active_file != prev_active {
            self.apply_scroll();
        }
        self.dismiss_popups();
        self.find_resync();
    }

    // ── Structure panel + Quick Open (Phase-4 wave 3) ─────────────────────────

    /// Switch the left sidebar between FILES and STRUCTURE (§5.5).
    pub fn set_sidebar_tab(&mut self, tab: SidebarTab) {
        self.sidebar_tab = tab;
    }

    /// Collapse/expand the left sidebar (§2; app.ts:449-459): 260px ⇄ 28px strip.
    pub fn toggle_sidebar(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.left_anim_gen += 1; // restart the width slide (28 ⇄ 260)
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

    // ── Find / replace (Ctrl+F / ⌘F) ──────────────────────────────────────────

    /// Open the find bar (or reveal the replace row on an already-open bar). Seeds
    /// the query from the current selection (a common editor nicety), scans the
    /// buffer for matches, and selects the one nearest the caret.
    pub fn open_find(&mut self, replace: bool) {
        // Nothing to search without an open tab.
        if self.editor.active_tab().is_none() {
            return;
        }
        // Seed the query from a non-empty single-line selection.
        let seed = self.editor.active_tab().and_then(|t| {
            let sel = t.buffer.selection();
            if sel.is_empty() {
                return None;
            }
            let s = t.buffer.to_string();
            let (a, b) = (sel.start(), sel.end());
            let text = s.get(a..b)?;
            if text.contains('\n') || text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        });

        match self.find.as_mut() {
            Some(state) => {
                // Already open: ⌘⌥F / Ctrl+H just jumps into the replace field.
                if replace {
                    state.show_replace();
                }
            }
            None => {
                // The replace row is always available (a dropdown the chevron can
                // collapse); `replace` only decides which field starts focused.
                let mut state = crate::find::FindState::new(true);
                state.field = if replace {
                    crate::find::FindField::Replace
                } else {
                    crate::find::FindField::Find
                };
                if let Some(q) = seed {
                    state.query = q;
                }
                state.move_end(); // caret after the seeded text
                self.find = Some(state);
            }
        }
        // Re-press of ⌘F (or opening fresh) grabs the field's focus.
        self.pending_find_focus = true;
        self.dismiss_popups();
        self.find_recompute();
        // Select the match nearest the caret so the first ↵/next lands sensibly.
        let caret = self
            .editor
            .active_tab()
            .map(|t| t.buffer.selection().caret())
            .unwrap_or(0);
        if let Some(state) = self.find.as_mut() {
            state.select_from(caret);
        }
        self.find_select_current();
    }

    /// Close the find bar and return keyboard focus to the editor.
    pub fn close_find(&mut self) {
        self.find = None;
        self.pending_editor_focus = true;
    }

    /// Click into one of the bar's text fields: make it the active field, put
    /// the caret at the clicked char column, and pull keyboard focus back to
    /// the bar (in case the editor had grabbed it).
    pub fn find_click_field(&mut self, field: crate::find::FindField, col: usize) {
        if let Some(state) = self.find.as_mut() {
            state.focus_field(field);
            state.set_cursor_col(col);
        }
        self.pending_find_focus = true;
        self.caret_activity();
    }

    /// Rescan the active buffer's matches if the find bar is open — the cheap
    /// "buffer text changed under the bar" hook (edits, undo/redo, tab
    /// switches). Without it the stored byte ranges go stale and the match
    /// washes render shifted off their words.
    pub fn find_resync(&mut self) {
        if self.find.is_some() {
            self.find_recompute();
        }
    }

    /// Rescan the active buffer for matches of the current query.
    pub fn find_recompute(&mut self) {
        let text = match self.editor.active_tab() {
            Some(t) => t.buffer.to_string(),
            None => return,
        };
        if let Some(state) = self.find.as_mut() {
            state.recompute(&text);
        }
    }

    /// Move the buffer selection onto the active match and scroll it into view.
    /// No-op when there is no current match (empty query / no hits).
    fn find_select_current(&mut self) {
        let Some(range) = self.find.as_ref().and_then(|s| s.current_range()) else {
            return;
        };
        if let Some(tab) = self.editor.active_tab_mut() {
            tab.unfold_containing(tab.buffer.offset_to_point(range.start).row);
            tab.buffer
                .set_selection(Selection::new(range.start, range.end));
        }
        self.caret_activity();
        // Center the match in the viewport (not just "nearest edge"), so the hit
        // lands mid-screen with context above and below.
        if let Some(tab) = self.editor.active_tab() {
            let row = tab.display_row(tab.buffer.offset_to_point(range.start).row);
            self.code_scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Center);
        }
    }

    /// Go to the next match (wrapping) and select it.
    pub fn find_next(&mut self) {
        if let Some(state) = self.find.as_mut() {
            state.next();
        }
        self.find_select_current();
    }

    /// Go to the previous match (wrapping) and select it.
    pub fn find_prev(&mut self) {
        if let Some(state) = self.find.as_mut() {
            state.prev();
        }
        self.find_select_current();
    }

    /// Toggle case-sensitivity, then rescan + reselect near the caret.
    pub fn find_toggle_case(&mut self) {
        let caret = self
            .editor
            .active_tab()
            .map(|t| t.buffer.selection().caret())
            .unwrap_or(0);
        if let Some(state) = self.find.as_mut() {
            state.toggle_case();
        }
        self.find_recompute();
        if let Some(state) = self.find.as_mut() {
            state.select_from(caret);
        }
        self.find_select_current();
    }

    /// Replace the active match with the replacement text, then advance to the
    /// next match past the insertion.
    pub fn find_replace_current(&mut self, cx: &mut Context<Self>) {
        let Some((range, replacement)) = self.find.as_ref().and_then(|s| {
            let r = s.current_range()?;
            Some((r, s.replace.clone()))
        }) else {
            return;
        };
        let record = self.with_edit(|b| b.edit(range.clone(), &replacement));
        self.after_edit(record, cx);
        // Rescan the changed text and jump to the first match past the insertion,
        // so a replacement that itself contains the query isn't re-hit forever.
        let resume = range.start + replacement.len();
        self.find_recompute();
        if let Some(state) = self.find.as_mut() {
            state.select_from(resume);
        }
        self.find_select_current();
    }

    /// Replace every match in one undo group. Selection collapses to the caret;
    /// the bar reports "0 of 0" afterwards (matches are gone unless the
    /// replacement re-introduces the needle).
    pub fn find_replace_all(&mut self, cx: &mut Context<Self>) {
        let Some(edits) = self.find.as_ref().and_then(|s| {
            if s.matches.is_empty() {
                return None;
            }
            let repl = s.replace.clone();
            let edits: Vec<(std::ops::Range<usize>, String)> =
                s.matches.iter().map(|r| (r.clone(), repl.clone())).collect();
            Some(edits)
        }) else {
            return;
        };
        let record = self.with_edit(|b| b.batch_edit(edits));
        self.after_edit(record, cx);
        self.find_recompute();
        if let Some(state) = self.find.as_mut() {
            state.current = None;
        }
    }

    /// Apply one captured keystroke to the find bar. `key` is the GPUI
    /// `Keystroke::key`; `key_char` the printable char (if any); `printable` gates
    /// character insertion (so ⌘/⌃/⌥ chords don't type). `shift`/`platform`
    /// refine a few bindings (⇧↵ = prev match, ⌘←/⌘→ = home/end). Returns
    /// nothing — the caller re-renders.
    pub fn find_key(
        &mut self,
        key: &str,
        key_char: Option<String>,
        printable: bool,
        shift: bool,
        platform: bool,
        cx: &mut Context<Self>,
    ) {
        if self.find.is_none() {
            return;
        }
        match key {
            "escape" => {
                self.close_find();
            }
            "enter" => {
                let on_replace = self
                    .find
                    .as_ref()
                    .map(|s| s.field == crate::find::FindField::Replace)
                    .unwrap_or(false);
                if on_replace && !shift {
                    // Enter in the replace field replaces the current match and
                    // advances; ⇧Enter still steps backward through matches.
                    self.find_replace_current(cx);
                } else if shift {
                    self.find_prev();
                } else {
                    self.find_next();
                }
            }
            "tab" => {
                if let Some(state) = self.find.as_mut() {
                    state.switch_field();
                }
            }
            "backspace" => {
                let query_changed = self.find.as_mut().map(|s| s.backspace()).unwrap_or(false);
                if query_changed {
                    self.find_recompute();
                    let caret = self
                        .editor
                        .active_tab()
                        .map(|t| t.buffer.selection().caret())
                        .unwrap_or(0);
                    if let Some(state) = self.find.as_mut() {
                        state.select_from(caret);
                    }
                    self.find_select_current();
                }
            }
            "delete" => {
                let query_changed = self
                    .find
                    .as_mut()
                    .map(|s| s.delete_forward())
                    .unwrap_or(false);
                if query_changed {
                    self.find_recompute();
                    self.find_next_from_caret();
                }
            }
            "left" => {
                if let Some(state) = self.find.as_mut() {
                    if platform {
                        state.move_home();
                    } else {
                        state.move_left();
                    }
                }
            }
            "right" => {
                if let Some(state) = self.find.as_mut() {
                    if platform {
                        state.move_end();
                    } else {
                        state.move_right();
                    }
                }
            }
            "home" => {
                if let Some(state) = self.find.as_mut() {
                    state.move_home();
                }
            }
            "end" => {
                if let Some(state) = self.find.as_mut() {
                    state.move_end();
                }
            }
            _ => {
                if printable {
                    if let Some(ch) = key_char {
                        if ch.chars().count() == 1 && !ch.chars().any(|c| c.is_control()) {
                            let query_changed =
                                self.find.as_mut().map(|s| s.type_str(&ch)).unwrap_or(false);
                            if query_changed {
                                self.find_recompute();
                                self.find_next_from_caret();
                            }
                        }
                    }
                }
            }
        }
        // Any handled key counts as caret activity → the field caret holds
        // steady-on instead of blinking away mid-interaction.
        self.caret_activity();
        let _ = cx;
    }

    /// After the query grows, reselect the first match at/after the caret (so
    /// typing incrementally walks matches forward like an editor's live search).
    fn find_next_from_caret(&mut self) {
        let caret = self
            .editor
            .active_tab()
            .map(|t| t.buffer.selection().start())
            .unwrap_or(0);
        if let Some(state) = self.find.as_mut() {
            state.select_from(caret);
        }
        self.find_select_current();
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
        // Discovery watchdog signals: decls = probe alive; scalars/timings =
        // the app is past startup and actually running (see
        // `discovery_should_stop`).
        if self.discovery_active {
            match &event {
                Event::Decl { .. } => self.discovery_decls = true,
                Event::Scalar(_) | Event::Timing(_) => {
                    self.discovery_warm_at
                        .get_or_insert_with(std::time::Instant::now);
                }
                _ => {}
            }
        }
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
                // A probe timer with the same name as a bundle def means the
                // def shadows a REAL timer (both write one registry key, and
                // the pre-run list hides def-named rows). The raw timer wins:
                // drop the def, keep the enabled pref so the series charts on.
                if kind == Kind::Timer && self.timer_groups.get(&name).is_some() {
                    self.timer_groups.remove(&name);
                    self.save_ui_state();
                    self.status_line(&format!(
                        "[jade] \"{name}\" is a probe timer — dropped the shadowing bundle"
                    ));
                }
                if out.auto_enabled {
                    self.server.set_track(kind, &name, true, None, None);
                }
                if out.pref_enabled {
                    self.push_track(kind, &name, true);
                } else if kind == Kind::Timer && self.timer_in_enabled_group(&name) {
                    // Bundle members are tracked for their group's sake even
                    // when individually unchecked (probe emits only on track).
                    self.push_track(kind, &name, true);
                }
                if kind == Kind::Timer {
                    self.migrate_stale_joined_timers();
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
                // Bundled members chart through their group's summed series;
                // the raw series only also lands when individually checked.
                let grouped = self.timer_groups.contains_member(&t.name);
                if !grouped || self.registry.is_enabled(Kind::Timer, &t.name) {
                    self.training.push_timing(&t.name, t.ms, t.step);
                }
                for (gname, sum, step) in self.timer_groups.on_sample(&t.name, t.ms, t.step) {
                    self.registry.note_timing(&gname, sum, step, &self.prefs);
                    self.training.push_timing(&gname, sum, step);
                }
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
        if kind == Kind::Timer {
            // Unchecking a bundled member must not untrack it in the probe
            // while an enabled bundle still consumes its samples.
            self.sync_member_tracking(name);
        } else {
            self.push_track(kind, name, now);
        }
    }

    // ── Timer bundles (pre-run panel "group as…") ─────────────────────────────

    /// Seed each loaded bundle's synthetic timer into the registry (its
    /// stored enabled-pref applies). Without this a persisted bundle was
    /// invisible to `timer_in_enabled_group` after a restart, so the decl
    /// handshake never re-armed member tracking — the probe stayed silent and
    /// the bundle never charted again until re-created.
    fn declare_loaded_timer_groups(&mut self) {
        let names: Vec<String> = self
            .timer_groups
            .defs()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        for name in names {
            self.registry.seed(Kind::Timer, &name, &self.prefs);
        }
    }

    /// Pre-populate the registry from every persisted enabled pref, so the
    /// telemetry sidebar and pre-run panel show last session's selections
    /// immediately — before any run or discovery scan has re-declared them.
    /// (Prefs are global, so a name last enabled in another project can show
    /// up too; unchecking it clears the pref for good.)
    fn seed_registry_from_prefs(&mut self) {
        let keys: Vec<String> = self
            .prefs
            .enabled
            .iter()
            .filter(|(_, on)| **on)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            let Some((kind_s, name)) = key.split_once(' ') else { continue };
            let kind = match kind_s {
                "scalar" => Kind::Scalar,
                "timer" => Kind::Timer,
                "buffer" => Kind::Buffer,
                _ => continue,
            };
            if !name.is_empty() {
                self.registry.seed(kind, name, &self.prefs);
            }
        }
    }

    /// Drop seeded rows the probe never confirmed, once a discovery scan or a
    /// full run has reported real inventory: at that point an unconfirmed
    /// seeded name refers to something that no longer exists (renamed kernel,
    /// re-symbolicated buffer, another project's leftovers) and only clutters
    /// the panels as a dead duplicate of its successor. Their enabled-prefs
    /// are cleared too so they stop re-seeding every launch. Bundle synthetic
    /// timers are exempt — they confirm via aggregation, not probe decls.
    fn prune_stale_seeded(&mut self) {
        // A probe that reported nothing (instant crash, no telemetry) proves
        // nothing about which names are stale — keep everything.
        let confirmed_any = [Kind::Scalar, Kind::Timer, Kind::Buffer]
            .iter()
            .any(|k| self.registry.items_of_kind(*k).iter().any(|i| !i.seeded));
        if !confirmed_any {
            return;
        }
        let bundle_names: std::collections::HashSet<String> = self
            .timer_groups
            .defs()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        let removed = self
            .registry
            .prune_seeded(move |kind, name| kind == Kind::Timer && bundle_names.contains(name));
        if removed.is_empty() {
            return;
        }
        for (kind, name) in &removed {
            self.prefs.set_enabled(&key_of(*kind, name), false);
        }
        self.prefs.save();
        self.status_line(&format!(
            "[jade] Cleared {} stale selection{} no longer reported by the probe",
            removed.len(),
            if removed.len() == 1 { "" } else { "s" }
        ));
    }

    /// Migrate stale '+'-joined timer selections to bundles. Command-buffer
    /// timers used to be named by joining their kernel names, so a checked
    /// "pipeline" from an older probe (or an older command-buffer layout) can
    /// be a joined name today's per-encoder probe never declares again — the
    /// row would sit seeded-and-checked forever with no data. Once EVERY
    /// kernel in such a name has been declared for real, recreate the
    /// selection as a timer bundle over those kernels: the summed series
    /// returns under the familiar name and the members stay individually
    /// selectable. Names whose parts never all appear (e.g. old truncated
    /// hashes) simply stay seeded and inert.
    fn migrate_stale_joined_timers(&mut self) {
        let candidates: Vec<(String, Vec<String>)> = self
            .registry
            .items_of_kind(Kind::Timer)
            .iter()
            .filter(|i| {
                i.seeded
                    && i.enabled
                    && i.name.contains('+')
                    && self.timer_groups.get(&i.name).is_none()
            })
            .map(|i| {
                (
                    i.name.clone(),
                    i.name.split('+').map(str::to_string).collect(),
                )
            })
            .collect();
        for (name, members) in candidates {
            let all_live = members.len() >= 2
                && members.iter().all(|m| {
                    self.registry
                        .get(Kind::Timer, m)
                        .is_some_and(|it| !it.seeded)
                });
            if all_live {
                self.create_timer_group(&name, members);
            }
        }
    }

    /// Whether `name` belongs to a bundle whose synthetic timer is checked.
    fn timer_in_enabled_group(&self, name: &str) -> bool {
        self.timer_groups
            .groups_of(name)
            .iter()
            .any(|g| self.registry.is_enabled(Kind::Timer, g))
    }

    /// A member must stay tracked in the probe while it's individually checked
    /// OR any enabled bundle needs its samples.
    fn sync_member_tracking(&self, member: &str) {
        let tracked =
            self.registry.is_enabled(Kind::Timer, member) || self.timer_in_enabled_group(member);
        self.push_track(Kind::Timer, member, tracked);
    }

    /// Create (or replace) a bundle from the panel's staged members, enable it,
    /// and arm tracking for every member. Persists both the definition
    /// (workspace state) and the enabled pref (under the synthetic name).
    pub fn create_timer_group(&mut self, name: &str, members: Vec<String>) {
        let name = name.trim();
        if name.is_empty() || members.is_empty() {
            return;
        }
        self.timer_groups.add(name, members.clone());
        // The synthetic series behaves like any timer: registry row + pref.
        self.registry.declare(Kind::Timer, name, None, None, &self.prefs);
        self.registry.set_enabled(Kind::Timer, name, true);
        self.prefs.set_enabled(&key_of(Kind::Timer, name), true);
        self.prefs.save();
        for m in &members {
            self.sync_member_tracking(m);
        }
        self.save_ui_state();
        self.status_line(&format!("[jade] Bundled {} timers as \"{name}\"", members.len()));
    }

    /// Dissolve a bundle: members stay in the registry (and stop being tracked
    /// unless individually checked); the synthetic series stops updating.
    pub fn dissolve_timer_group(&mut self, name: &str) {
        let Some(def) = self.timer_groups.get(name) else {
            return;
        };
        let members = def.members.clone();
        self.timer_groups.remove(name);
        self.registry.set_enabled(Kind::Timer, name, false);
        self.prefs.set_enabled(&key_of(Kind::Timer, name), false);
        self.prefs.save();
        for m in &members {
            self.sync_member_tracking(m);
        }
        self.save_ui_state();
    }

    /// Toggle a bundle's checkbox: flips the synthetic series' enabled state
    /// and re-arms/releases every member's probe tracking accordingly.
    pub fn toggle_timer_group(&mut self, name: &str) {
        self.toggle_enabled(Kind::Timer, name);
        if let Some(def) = self.timer_groups.get(name) {
            for m in def.members.clone() {
                self.sync_member_tracking(&m);
            }
        }
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

    // ── Buffer shape hint / streaming-cap editors (§7.2, §5.6) ────────────────
    // Ported from `weight-grid-3d.ts`'s dim inputs + res select and
    // `telemetry-panel.ts`'s inline shape editor. Both surfaces (the wg3d
    // toolbar and the telemetry sidebar row) drive the SAME `dim_edit` session
    // through these methods; only the paint differs per file.

    /// Current dim-edit session, if any (read-only accessor for the renderers).
    pub fn dim_edit(&self) -> Option<&dim_input::DimEditState> {
        self.dim_edit.as_ref()
    }

    /// Open the rows×cols editor for `name`, prefilled from the stored hint
    /// (else empty; the caller shows a placeholder via [`Self::dim_edit_placeholder`]).
    /// A second call for the SAME buffer toggles it closed (`toggleShapeEditor`'s
    /// "second click on same row = close" rule); a call for a different buffer
    /// replaces the open session — only one editor is open at a time.
    pub fn start_dim_edit(&mut self, name: &str, field: dim_input::DimField) {
        if let Some(cur) = &self.dim_edit {
            if cur.name == name {
                self.dim_edit = None;
                return;
            }
        }
        let item = self.registry.get(Kind::Buffer, name);
        let rows = item.and_then(|i| i.shape_rows).map(|r| r.to_string()).unwrap_or_default();
        let cols = item.and_then(|i| i.shape_cols).map(|c| c.to_string()).unwrap_or_default();
        self.dim_edit = Some(dim_input::DimEditState::new(name.to_string(), rows, cols, field));
    }

    /// Switch which field (rows/cols) has the caret without closing the editor
    /// (clicking the other field mid-edit).
    pub fn set_dim_edit_field(&mut self, field: dim_input::DimField) {
        if let Some(s) = &mut self.dim_edit {
            s.field = field;
        }
    }

    /// Placeholder shown in an empty rows/cols field: the buffer's latest known
    /// source dimensions (`syncDimInputs`'s placeholder fallback). `note_tensor`
    /// already registers a buffer under its SOURCE rows/cols, so `last_rows`/
    /// `last_cols` are exactly the values the TS read off the latest frame —
    /// no separate wg3d frame lookup needed.
    pub fn dim_edit_placeholder(&self, name: &str) -> (String, String) {
        let item = self.registry.get(Kind::Buffer, name);
        let r = item.and_then(|i| i.last_rows.or(i.meta_rows));
        let c = item.and_then(|i| i.last_cols.or(i.meta_cols));
        (
            r.map(|v| v.to_string()).unwrap_or_default(),
            c.map(|v| v.to_string()).unwrap_or_default(),
        )
    }

    /// The persistent focus handle for the open dim editor (used by both the
    /// wg3d toolbar and the telemetry sidebar's inline editor — whichever is
    /// showing it). `None` until a session has been opened at least once, or
    /// headless.
    pub fn dim_edit_focus_handle(&self) -> Option<FocusHandle> {
        self.dim_edit_focus.clone()
    }

    /// Apply one captured keystroke to the open dim editor. Returns `true` if
    /// consumed (the caller should `cx.stop_propagation()`), mirroring
    /// `bench_key`'s contract.
    pub fn dim_edit_key(&mut self, ks: &gpui::Keystroke) -> bool {
        let Some(state) = self.dim_edit.as_mut() else {
            return false;
        };
        let m = ks.modifiers;
        let printable = ks.key_char.is_some() && !m.platform && !m.control && !m.alt && !m.function;
        let key = ks.key.as_str();
        if !printable && !matches!(key, "escape" | "enter" | "return" | "backspace" | "tab") {
            return false;
        }
        match state.on_key(key, ks.key_char.as_deref(), printable) {
            dim_input::DimKeyAction::None => {}
            dim_input::DimKeyAction::Cancel => self.dim_edit = None,
            dim_input::DimKeyAction::Commit(r, c) => {
                let name = self.dim_edit.as_ref().unwrap().name.clone();
                self.commit_dim_shape(&name, r, c);
                self.dim_edit = None;
            }
        }
        true
    }

    /// Apply the in-progress edit if both fields are valid ints ≥ 1 — same
    /// rule Enter uses, exposed for the sidebar's "set" button (the TS wires
    /// both Enter and the apply button to the same `apply()`/`commit`).
    pub fn commit_dim_edit_if_valid(&mut self) {
        let Some((r, c)) = self.dim_edit.as_ref().and_then(|s| s.try_commit()) else {
            return;
        };
        let name = self.dim_edit.as_ref().unwrap().name.clone();
        self.commit_dim_shape(&name, r, c);
        self.dim_edit = None;
    }

    /// Apply a committed rows×cols hint: persist it, and — if the buffer is
    /// currently enabled — re-push `track` immediately so the probe re-streams
    /// with the new shape (weight-grid-3d.ts `applyShape` / telemetry-panel.ts
    /// `apply`, both via `telemetryRegistry.setShape`, which forwards to the
    /// server on the next `track`).
    fn commit_dim_shape(&mut self, name: &str, rows: u32, cols: u32) {
        if !self.registry.set_shape(Kind::Buffer, name, rows, cols) {
            return;
        }
        let key = key_of(Kind::Buffer, name);
        self.prefs.set_shape(&key, rows, cols);
        self.prefs.save();
        if self.registry.is_enabled(Kind::Buffer, name) {
            self.push_track(Kind::Buffer, name, true);
        }
    }

    /// User picked a streaming-resolution chip (≤64/≤128/≤256) for a buffer in
    /// the 3D toolbar (`resSelect`'s change handler): persist the registry's
    /// per-buffer maxDim and re-push `track` if enabled, so the probe
    /// re-streams at the new cap immediately.
    pub fn set_buffer_max_dim(&mut self, name: &str, max_dim: u32) {
        if !self.registry.set_max_dim(Kind::Buffer, name, max_dim) {
            return;
        }
        let key = key_of(Kind::Buffer, name);
        self.prefs.set_maxdim(&key, max_dim);
        self.prefs.save();
        if self.registry.is_enabled(Kind::Buffer, name) {
            self.push_track(Kind::Buffer, name, true);
        }
    }
}

/// Char advance of Menlo at the editor font size (mouse↔column mapping). See the
/// note on `panels::code_view::CHAR_W`.
use crate::panels::code_view::LINE_H;

/// Resolve the cwd for a new terminal (§5.2). A selected directory wins; a
/// selected file contributes its parent directory. The active file's directory
/// is the next choice, and `root` is the last one. A path that no longer exists
/// (a deleted directory, for example) falls through to the next choice, so the
/// shell always starts somewhere real.
fn terminal_cwd_for(selection: Option<&Path>, active: Option<&Path>, root: &Path) -> PathBuf {
    let dir_of = |p: &Path| -> Option<PathBuf> {
        if p.is_dir() {
            Some(p.to_path_buf())
        } else {
            p.parent().filter(|d| d.is_dir()).map(|d| d.to_path_buf())
        }
    };
    selection
        .and_then(dir_of)
        .or_else(|| active.and_then(dir_of))
        .unwrap_or_else(|| root.to_path_buf())
}

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
/// `JADE_GHOST_LOG=1`: trace every ghost-pipeline decision to stderr, so a
/// "no ghost text" report can say exactly which stage killed the suggestion
/// (gate, stale generation, empty model output, post-process suppression).
fn ghost_log(f: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("JADE_GHOST_LOG").is_some()) {
        eprintln!("[ghost] {}", f());
    }
}

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

    /// Close the autocomplete + hover popups, the signature hint, and any ghost.
    pub fn dismiss_popups(&mut self) {
        self.completion = None;
        self.hover = None;
        self.hover_target = None;
        self.signature = None;
        self.ghost = None;
    }

    /// Dispatch one editor keystroke; returns true when consumed (the caller then
    /// stops propagation so the key doesn't bubble / reach the IME). Character
    /// keys return false so macOS routes them to `replace_text_in_range`.
    pub fn editor_key(&mut self, ks: &gpui::Keystroke, cx: &mut Context<Self>) -> bool {
        let m = ks.modifiers;
        let key = ks.key.as_str();
        let shift = m.shift;

        // Find / replace open (⌘F, ⌘⌥F, Ctrl+F, Ctrl+H). Handled up front so the
        // ⌥/⌃ variants aren't swallowed by the plain-chord blocks below, and so it
        // works regardless of any open popup. ⌥ (or the H binding) reveals replace.
        if !shift {
            if key == "f" && (m.platform || m.control) {
                self.open_find(m.alt);
                return true;
            }
            if key == "h" && m.control && !m.alt {
                self.open_find(true);
                return true;
            }
        }

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

        // Sync-suggestion banner (§4.13): ⌘⏎ applies, Esc dismisses. After the
        // ghost gate so Esc clears a ghost first.
        if self.sync_suggestion.is_some() {
            if key == "escape" && !m.platform && !m.control && !m.alt {
                self.sync_suggestion = None;
                return true;
            }
            if key == "enter" && m.platform && !m.control && !m.alt {
                self.sync_apply(cx);
                return true;
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
        if self.signature.is_some() && key == "escape" {
            self.signature = None;
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
                "c" => {
                    // ⌘⇧C hands the file off to CLion at the caret; ⌘C copies.
                    if shift {
                        self.action_open_in_clion();
                    } else {
                        self.editor_copy(cx);
                    }
                }
                "x" => self.editor_cut(cx),
                "v" => self.editor_paste(cx),
                "left" => self.smart_home(shift),
                "right" => self.buf_move(|b, e| b.move_end(e), shift),
                "up" => self.buf_move(|b, e| b.move_doc_start(e), shift),
                "down" => self.buf_move(|b, e| b.move_doc_end(e), shift),
                "backspace" => {
                    // ⌘⌫: delete the whole line(s) spanned by the selection.
                    let r = self.with_edit(|b| {
                        let sel = b.selection();
                        let sr = b.offset_to_point(sel.start()).row;
                        let er = b.offset_to_point(sel.end()).row;
                        let start = b.point_to_offset(Point::new(sr, 0));
                        let end = if er + 1 < b.line_count() {
                            b.point_to_offset(Point::new(er + 1, 0))
                        } else {
                            b.len_bytes()
                        };
                        b.edit(start..end, "")
                    });
                    self.after_edit(r, cx);
                }
                _ => return false, // let ⌘P etc. bubble
            }
            return true;
        }

        // ⌥ chords (word nav + word delete). With a ghost up, ⌥→ accepts just
        // its next word (JetBrains FLCC partial accept) instead of moving.
        if m.alt && !m.platform {
            match key {
                "left" => self.buf_move(|b, e| b.move_word_left(e), shift),
                "right" if self.ghost.is_some() && !shift => self.ghost_accept_word(cx),
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
            "home" => self.smart_home(shift),
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
    /// Toggle the fold at `row` (a fold-map start). Folding a region that
    /// contains the caret first hoists the caret to the fold's start line so
    /// it never sits in a hidden row.
    pub fn toggle_fold(&mut self, row: usize) {
        let Some(tab) = self.editor.active_tab_mut() else {
            return;
        };
        if !tab.fold_map.contains_key(&row) {
            return;
        }
        if tab.folds.contains(&row) {
            tab.folds.remove(&row);
        } else {
            if let Some(&end) = tab.fold_map.get(&row) {
                let caret_row = tab.caret_point().row;
                if caret_row > row && caret_row < end {
                    let start = tab.buffer.point_to_offset(Point::new(row, 0));
                    tab.buffer.set_caret(start);
                }
            }
            tab.folds.insert(row);
        }
        self.caret_activity();
    }

    /// After any caret move/edit: if the caret landed inside a folded region
    /// (arrow through a fold, undo, goto), unfold it so the caret stays visible.
    fn unfold_at_caret(&mut self) {
        if let Some(tab) = self.editor.active_tab_mut() {
            let row = tab.caret_point().row;
            tab.unfold_containing(row);
        }
    }

    /// Line-aware Home (⌘←/Home): first press jumps to the text edge (first
    /// non-whitespace char); pressing again from there goes to column 0.
    fn smart_home(&mut self, extend: bool) {
        self.dismiss_popups();
        {
            let Some(tab) = self.editor.active_tab_mut() else {
                return;
            };
            let caret = tab.caret_point();
            let line = tab.buffer.line(caret.row).into_owned();
            let first_ns = line
                .chars()
                .position(|c| !c.is_whitespace())
                .unwrap_or(0);
            let target_col = if caret.col == first_ns { 0 } else { first_ns };
            let byte = tab.buffer.point_to_offset(Point::new(caret.row, target_col));
            if extend {
                let anchor = tab.buffer.selection().anchor;
                tab.buffer.set_selection(Selection::new(anchor, byte));
            } else {
                tab.buffer.set_caret(byte);
            }
        }
        self.caret_activity();
        self.scroll_caret_into_view();
        self.sync_asm_selection();
    }

    /// Any caret movement or edit: hold the caret solid for the next blink
    /// window so it never flickers while typing/navigating.
    fn caret_activity(&mut self) {
        self.caret_last_active = self.now_ms();
        self.caret_blink_show = true;
    }

    fn buf_move(&mut self, f: impl FnOnce(&mut jade_buffer::Buffer, bool), extend: bool) {
        if let Some(tab) = self.editor.active_tab_mut() {
            f(&mut tab.buffer, extend);
        }
        self.unfold_at_caret();
        self.caret_activity();
        self.completion = None;
        self.hover = None;
        self.hover_target = None;
        self.ghost = None; // any caret move dismisses ghost text (§4.11)
        // Signature help follows the caret: while a hint is up, re-request so
        // moving between arguments updates the active parameter, and clangd
        // returning nothing (caret left the call) dismisses it.
        if self.signature.is_some() {
            self.schedule_signature_help();
        }
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
        f: impl FnOnce(&mut jade_buffer::Buffer) -> jade_buffer::EditRecord,
    ) -> jade_buffer::EditRecord {
        match self.editor.active_tab_mut() {
            Some(tab) => f(&mut tab.buffer),
            None => jade_buffer::EditRecord {
                changes: Vec::new(),
                version: 0,
            },
        }
    }

    /// Shared post-edit bookkeeping: recompute eager decorations + arm debounces,
    /// forward an incremental `didChange`, follow the caret, wake the debounce.
    fn after_edit(&mut self, record: jade_buffer::EditRecord, cx: &mut Context<Self>) {
        self.unfold_at_caret();
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
        // Keep the find bar's match ranges in sync with the buffer: without this
        // any edit made while the bar is open (a typed space, Tab accepting a
        // ghost…) leaves stale byte ranges and the match washes slide off their
        // words. `recompute` keeps `current` on the same-or-nearest match.
        self.find_resync();
        // XP credit for newline-completing edits (§4.10), then (re)request ghost
        // text at the new caret (§4.11). Both hang off this single choke point.
        self.credit_xp(&record, now);
        self.scroll_caret_into_view();
        self.schedule_ghost();
        self.ensure_decoration_wake(cx);
        // ASM viewer (§6): keep the cross-highlight aligned with the caret and
        // auto-refresh the listing 1.5s after the edit settles while visible.
        self.sync_asm_selection();
        self.schedule_asm_refresh(cx);
    }

    /// Award XP for edits whose change inserted a newline and whose completed line
    /// (comments stripped) ends with `;`, ≤300 chars (§4.10). The streak scales
    /// the award and persists globally.
    fn credit_xp(&mut self, record: &jade_buffer::EditRecord, now_ms: u64) {
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

    /// The code list's horizontal scroll offset in px (≤ 0; more negative the
    /// further right you've scrolled). The code text is painted shifted by this,
    /// so click→column mapping must subtract it and popups must add it to stay
    /// aligned with the on-screen glyphs.
    pub fn editor_h_scroll(&self) -> f32 {
        f32::from(self.code_scroll.0.borrow().base_handle.offset().x)
    }

    /// Snapshot the live scroll position into the active tab (called just before
    /// leaving it — a tab switch, a project switch, or opening another file — so
    /// its page position is remembered). Reads the current *painted* offset, so
    /// it must run before any deferred `scroll_to_item` for the new tab.
    fn stash_scroll(&mut self) {
        let top = self.editor_scroll_top();
        if let Some(tab) = self.editor.active_tab_mut() {
            tab.scroll_top = top;
        }
    }

    /// Scroll the code list to the active tab's remembered page position (a
    /// deferred scroll honored on the next paint). Called after switching to a tab.
    fn apply_scroll(&mut self) {
        let top = self.editor.active_tab().map(|t| t.scroll_top).unwrap_or(0);
        self.code_scroll
            .scroll_to_item(top, gpui::ScrollStrategy::Top);
    }

    /// Minimal follow-scroll: bring the caret row into the visible window only
    /// when it's currently outside it (no jarring recentre on every keystroke).
    fn scroll_caret_into_view(&mut self) {
        let Some(tab) = self.editor.active_tab() else {
            return;
        };
        // Scroll space is DISPLAY rows (folds hide buffer rows).
        let row = tab.display_row(tab.caret_point().row);
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
                    let mut sync_fired = Vec::new();
                    for (i, tab) in app.editor.tabs.iter_mut().enumerate() {
                        changed |= tab.poll_decorations(now);
                        if tab.sync_debounce.poll(now) {
                            sync_fired.push(i);
                        }
                    }
                    for i in sync_fired {
                        changed |= app.sync_detect(i);
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

    // ── Cross-file sync (§4.13) ───────────────────────────────────────────────

    /// Run the sync detectors for tab `idx` against its snapshot, then refresh
    /// the snapshot. Returns true when a new suggestion appeared.
    pub(crate) fn sync_detect(&mut self, idx: usize) -> bool {
        use crate::sync;
        let (path, old_text, new_text) = {
            let Some(tab) = self.editor.tabs.get_mut(idx) else {
                return false;
            };
            let new_text = tab.buffer.to_string();
            let old_text = std::mem::replace(&mut tab.sync_snapshot, new_text.clone());
            (tab.path.clone(), old_text, new_text)
        };
        if old_text == new_text {
            return false;
        }

        // 1. Kernel rename in a .metal file → host references (string literals
        // plus derived identifiers such as `residualAddPipeline`).
        if sync::is_metal(&path) {
            let old_k = sync::scan_kernel_names(&old_text);
            let new_k = sync::scan_kernel_names(&new_text);
            if let Some((from, to)) = sync::detect_rename(&old_k, &new_k) {
                // Chain across debounce windows: the host still references the
                // name from before the FIRST fire, not the snapshot's
                // intermediate name. Without this only the first change of the
                // rename could ever match a host reference.
                let mut chained = false;
                let from = match &self.sync_suggestion {
                    Some(sync::SyncSuggestion::RenameKernel { old, new, .. }) if *new == from => {
                        chained = true;
                        old.clone()
                    }
                    _ => from,
                };
                if from == to {
                    // The user typed the rename back to the original name.
                    if chained {
                        self.sync_suggestion = None;
                    }
                    return chained;
                }
                let scope = self.sync_scope_dir();
                let (mut refs, mut files) = (0usize, 0usize);
                for f in self.sync_workspace_files(&scope) {
                    if !sync::is_host_source(&f) {
                        continue;
                    }
                    let Some(text) = self.sync_read(&f) else {
                        continue;
                    };
                    let n = sync::kernel_ref_sites(&text, &from).len();
                    if n > 0 {
                        refs += n;
                        files += 1;
                    }
                }
                if refs > 0 {
                    self.sync_scope = scope;
                    self.sync_suggestion = Some(sync::SyncSuggestion::RenameKernel {
                        old: from,
                        new: to,
                        refs,
                        files,
                    });
                    return true;
                }
                // No references: a chained suggestion is now stale.
                if chained {
                    self.sync_suggestion = None;
                }
                return chained;
            }
        }

        // 2. Hyperparameter value change → other declaration sites.
        if sync::is_hyperparam_file(&path) {
            let old_d = sync::scan_hyperparams(&old_text, &path);
            let new_d = sync::scan_hyperparams(&new_text, &path);
            if let Some((name, _from, to)) = sync::detect_value_change(&old_d, &new_d) {
                let scope = self.sync_scope_dir();
                let (mut sites, mut files) = (0usize, 0usize);
                for f in self.sync_workspace_files(&scope) {
                    if f == path || !sync::is_hyperparam_file(&f) {
                        continue;
                    }
                    let Some(text) = self.sync_read(&f) else {
                        continue;
                    };
                    let n = sync::scan_hyperparams(&text, &f)
                        .iter()
                        .filter(|d| d.name == name && d.value != to)
                        .count();
                    if n > 0 {
                        sites += n;
                        files += 1;
                    }
                }
                if sites > 0 {
                    self.sync_scope = scope;
                    self.sync_suggestion =
                        Some(sync::SyncSuggestion::Hyperparam { name, to, sites, files });
                    return true;
                }
                // No sites left (for example the user typed the value back):
                // drop a pending suggestion for the same name.
                if matches!(
                    &self.sync_suggestion,
                    Some(sync::SyncSuggestion::Hyperparam { name: n, .. }) if *n == name
                ) {
                    self.sync_suggestion = None;
                    return true;
                }
                return false;
            }
        }

        // 3. One token changed on one line → other occurrences in this file.
        if let Some((_row, la, lb)) = sync::single_line_diff(&old_text, &new_text) {
            if let Some((from, to)) = sync::single_token_diff(la, lb) {
                // Chain across debounce windows (same reason as the kernel
                // rename above): anchor on the token the file still holds.
                let mut chained = false;
                let from = match &self.sync_suggestion {
                    Some(sync::SyncSuggestion::SimilarLines { from: orig, to: prev, .. })
                        if *prev == from =>
                    {
                        chained = true;
                        orig.clone()
                    }
                    _ => from,
                };
                if from == to {
                    if chained {
                        self.sync_suggestion = None;
                    }
                    return chained;
                }
                let count = sync::token_sites(&new_text, &from).len();
                if count > 0 {
                    self.sync_suggestion =
                        Some(sync::SyncSuggestion::SimilarLines { from, to, count });
                    return true;
                }
                if chained {
                    self.sync_suggestion = None;
                }
                return chained;
            }
        }
        false
    }

    /// Apply the pending sync suggestion (⌘⏎ or the banner button).
    pub fn sync_apply(&mut self, cx: &mut Context<Self>) {
        use crate::sync;
        let Some(sug) = self.sync_suggestion.take() else {
            return;
        };
        match sug {
            sync::SyncSuggestion::SimilarLines { from, to, .. } => {
                // One batch edit on the active buffer: one undo group.
                let record = self.with_edit(|b| {
                    b.group_boundary();
                    let text = b.to_string();
                    let edits = sync::token_sites(&text, &from)
                        .into_iter()
                        .map(|r| (r, to.clone()))
                        .collect();
                    b.batch_edit(edits)
                });
                self.after_edit(record, cx);
                if let Some(tab) = self.editor.active_tab_mut() {
                    tab.sync_snapshot = tab.buffer.to_string();
                }
            }
            sync::SyncSuggestion::RenameKernel { old, new, .. } => {
                let scope = self.sync_scope.clone();
                for f in self.sync_workspace_files(&scope) {
                    if !sync::is_host_source(&f) {
                        continue;
                    }
                    let (old, new) = (old.clone(), new.clone());
                    self.sync_rewrite_file(
                        &f,
                        move |text, _| {
                            sync::kernel_ref_sites(text, &old)
                                .into_iter()
                                .map(|r| (r, new.clone()))
                                .collect()
                        },
                        cx,
                    );
                }
            }
            sync::SyncSuggestion::Hyperparam { name, to, .. } => {
                let scope = self.sync_scope.clone();
                for f in self.sync_workspace_files(&scope) {
                    if !sync::is_hyperparam_file(&f) {
                        continue;
                    }
                    let (name, to) = (name.clone(), to.clone());
                    self.sync_rewrite_file(
                        &f,
                        move |text, path| {
                            sync::scan_hyperparams(text, path)
                                .into_iter()
                                .filter(|d| d.name == name && d.value != to)
                                .map(|d| (d.value_range, to.clone()))
                                .collect()
                        },
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    /// The directory the cross-file sync scans: the directory selected in the
    /// file tree (a selected file does not narrow the scope), else the
    /// workspace root.
    fn sync_scope_dir(&self) -> PathBuf {
        match &self.tree_selection {
            Some(p) if p.is_dir() => p.clone(),
            _ => self.workspace_root.clone(),
        }
    }

    /// True when the pending suggestion's scan covered less than the full
    /// workspace (the banner then names the scope directory).
    pub fn sync_scope_is_narrow(&self) -> bool {
        self.sync_scope != self.workspace_root
    }

    /// All file paths under `scope` the sync detectors may scan (capped).
    fn sync_workspace_files(&self, scope: &Path) -> Vec<PathBuf> {
        let tree = FileTree::scan_full(scope.to_path_buf());
        crate::quick_open::flatten(&tree)
            .into_iter()
            .map(|e| e.path)
            .take(4000)
            .collect()
    }

    /// The current text of `path`: the open buffer when the file is open in a
    /// tab, else the on-disk content (≤1 MB).
    fn sync_read(&self, path: &Path) -> Option<String> {
        if let Some(tab) = self.editor.tabs.iter().find(|t| t.path == *path) {
            return Some(tab.buffer.to_string());
        }
        let meta = std::fs::metadata(path).ok()?;
        if meta.len() > 1_000_000 {
            return None;
        }
        let raw = std::fs::read(path).ok()?;
        Some(String::from_utf8_lossy(&raw).into_owned())
    }

    /// Apply computed edits to one file. An open tab gets a buffer batch edit
    /// (undoable, marks the tab dirty, notifies clangd). A closed file gets a
    /// direct disk rewrite. Returns the number of applied edits.
    fn sync_rewrite_file(
        &mut self,
        path: &Path,
        edits_for: impl Fn(&str, &Path) -> Vec<(std::ops::Range<usize>, String)>,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(i) = self.editor.tabs.iter().position(|t| t.path == *path) {
            let now = self.now_ms();
            let (record, n, payload) = {
                let tab = &mut self.editor.tabs[i];
                let text = tab.buffer.to_string();
                let edits = edits_for(&text, path);
                if edits.is_empty() {
                    return 0;
                }
                let n = edits.len();
                tab.buffer.group_boundary();
                let record = tab.buffer.batch_edit(edits);
                tab.on_edited(now);
                tab.sync_snapshot = tab.buffer.to_string();
                let payload = (
                    tab.path.clone(),
                    tab.lsp_opened,
                    tab.lsp_version(),
                    tab.buffer.to_string(),
                );
                (record, n, payload)
            };
            let (p, opened, version, text) = payload;
            if opened && !record.is_noop() {
                let edits = editor_view::record_to_lsp_edits(&record);
                self.lsp_did_change(&p, edits, text, version);
            }
            self.ensure_decoration_wake(cx);
            return n;
        }
        // Closed file: splice the edits into the on-disk text.
        let Ok(raw) = std::fs::read(path) else {
            return 0;
        };
        let text = String::from_utf8_lossy(&raw).into_owned();
        let mut edits = edits_for(&text, path);
        if edits.is_empty() {
            return 0;
        }
        edits.sort_by_key(|(r, _)| r.start);
        let n = edits.len();
        let mut out = String::with_capacity(text.len());
        let mut last = 0usize;
        for (r, rep) in edits {
            out.push_str(&text[last..r.start]);
            out.push_str(&rep);
            last = r.end;
        }
        out.push_str(&text[last..]);
        let _ = std::fs::write(path, out);
        n
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
                self.status_line(&format!("[jade] Saved {}", path.display()));
            }
            Err(e) => self.status_line(&format!("[jade] Save failed: {e}")),
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
        self.find_resync(); // undo/redo bypasses after_edit
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
        // Fold in the horizontal scroll so click→column and caret geometry track
        // the glyphs after the code list scrolls sideways (the text_left canvas
        // captures a fixed left edge; the content shifts by `editor_h_scroll`).
        let text_left =
            f32::from_bits(self.editor_text_left.load(Ordering::Relaxed)) + self.editor_h_scroll();
        let cw = self.char_w();
        let selecting = {
            let Some(tab) = self.editor.active_tab_mut() else {
                return;
            };
            let row = row.min(tab.line_count().saturating_sub(1));
            let line = tab.buffer.line(row).into_owned();
            let col = editor_view::px_to_char_col(&line, x - text_left, cw);
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
        // Fold in the horizontal scroll so click→column and caret geometry track
        // the glyphs after the code list scrolls sideways (the text_left canvas
        // captures a fixed left edge; the content shifts by `editor_h_scroll`).
        let text_left =
            f32::from_bits(self.editor_text_left.load(Ordering::Relaxed)) + self.editor_h_scroll();
        let cw = self.char_w();
        {
            let Some(tab) = self.editor.active_tab_mut() else {
                return;
            };
            let row = row.min(tab.line_count().saturating_sub(1));
            let col = editor_view::px_to_char_col(&tab.buffer.line(row), x - text_left, cw);
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
    /// Independently drives signature help: `(` / `,` open or advance the
    /// parameter hint, `)` dismisses it.
    fn on_text_inserted(&mut self, inserted: &str, cx: &mut Context<Self>) {
        let last = inserted.chars().last();

        // Signature help (parameter hints) — independent of the completion popup.
        match last {
            Some('(') | Some(',') => self.schedule_signature_help(),
            Some(')') => self.signature = None,
            _ => {}
        }

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
            let position = jade_lsp::Position::new(pos.line as u32, pos.character as u32);
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

    /// Request signature help at the caret (parameter hints for the call being
    /// filled in). Debounced like completion; the generation counter supersedes
    /// stale responses. Only on clangd-eligible files with an LSP session.
    /// True once the clangd session is live (test/smoke seam).
    pub fn lsp_active(&self) -> bool {
        self.lsp.is_some()
    }

    pub fn schedule_signature_help(&mut self) {
        self.sig_help_requests += 1;
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
        self.signature_gen += 1;
        let generation = self.signature_gen;
        let tx = self.app_tx.clone();
        let anchor = (point.row, point.col);
        self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let position = jade_lsp::Position::new(pos.line as u32, pos.character as u32);
            let hint = match lsp.signature_help(&path, position).await {
                Ok(Some(help)) => active_signature_hint(&help),
                _ => None,
            };
            let _ = tx.send(AppEvent::SignatureHelp {
                generation,
                hint,
                anchor,
            });
        });
    }

    fn on_signature_help(
        &mut self,
        generation: u64,
        hint: Option<SignatureHint>,
        anchor: (usize, usize),
    ) {
        if generation != self.signature_gen {
            return; // superseded
        }
        self.signature = hint.map(|h| SignatureState {
            label: h.label,
            active_param: h.active_param,
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
    /// immediately clears any visible suggestion. Like the old Jade, this also
    /// brings the managed backend up (enable) or tears it down (disable) so the
    /// server releases GPU/unified memory when AI is off (`app.ts:292`).
    pub fn action_toggle_ai_completion(&mut self) {
        self.ai_completion_enabled = !self.ai_completion_enabled;
        if !self.ai_completion_enabled {
            self.ghost = None;
        }
        let ai = self.ai.clone();
        let enable = self.ai_completion_enabled;
        self.runtime.spawn(async move {
            if enable {
                ai.start().await;
            } else {
                ai.stop().await;
            }
        });
    }

    /// Bring the managed AI backend up if completion is enabled but the server
    /// isn't running yet (state `Disabled`/`Error`). Idempotent — a no-op once
    /// it's `Starting`/`Ready`. This is why clicking the sparkle "just works":
    /// on launch `ai_completion_enabled` defaults on but nothing had started the
    /// server, so ghost text stayed dead until you toggled the switch off and on.
    pub fn ensure_ai_started(&mut self) -> bool {
        if !self.ai_completion_enabled {
            return false;
        }
        if matches!(self.ai_status.state, AiState::Disabled | AiState::Error) {
            let ai = self.ai.clone();
            self.runtime.spawn(async move {
                ai.start().await;
            });
            return true;
        }
        false
    }

    /// Open/close the sparkle AI settings menu (completion · multiline · model).
    /// Opening also starts the backend if completion is on but idle, so the
    /// sparkle alone brings AI up.
    pub fn toggle_ai_menu(&mut self) {
        self.ai_menu_open = !self.ai_menu_open;
        if self.ai_menu_open {
            self.ensure_ai_started();
        }
    }

    /// Dismiss the AI settings menu (outside click / after a choice).
    pub fn close_ai_menu(&mut self) {
        self.ai_menu_open = false;
    }

    /// Select the managed-model tier (`aiModel`): updates the shown selection,
    /// persists it globally (`~/.config/jade/ai.json`), and applies it to the
    /// backend, which restarts the managed server if one is running
    /// (`jade_ai::set_model`; a no-op when the tier is unchanged).
    pub fn set_ai_model(&mut self, id: AiModelId) {
        self.ai_model = id;
        self.ai_prefs.model = id;
        self.ai_prefs.save();
        let ai = self.ai.clone();
        self.runtime.spawn(async move {
            ai.set_model(id).await;
        });
    }

    /// Toggle multiline ghost mode (`aiMultiline`). Cached output was generated
    /// for the old mode, so the cache is cleared (§4.11). The choice persists
    /// globally (`~/.config/jade/ai.json`), matching the old Jade.
    pub fn action_toggle_ai_multiline(&mut self) {
        self.ai_multiline = !self.ai_multiline;
        self.ghost_cache.clear();
        self.ghost = None;
        self.ai_prefs.multiline = self.ai_multiline;
        self.ai_prefs.save();
    }

    /// (Re)compute ghost text at the caret (§4.11): serve a cache hit instantly
    /// (exact or typed-through), else debounce 120ms and request `/infill`
    /// (jade-ai's single-flight aborts any older request). Only fires when
    /// `aiCompletionEnabled` and the backend is `Ready` at a collapsed caret.
    fn schedule_ghost(&mut self) {
        if !self.ai_completion_enabled {
            self.ghost = None;
            return;
        }
        if self.ai_status.state != AiState::Ready {
            // Typing is the clearest signal completions are wanted: wake a
            // Disabled/Error backend so the *next* pause gets a ghost.
            self.ensure_ai_started();
            ghost_log(|| format!("skip: backend {:?}", self.ai_status.state));
            self.ghost = None;
            return;
        }
        let Some(tab) = self.editor.active_tab() else {
            self.ghost = None;
            return;
        };
        if !ghost_eligible(&tab.path) || !tab.buffer.selection().is_empty() {
            ghost_log(|| {
                format!(
                    "skip: eligible={} selection_empty={}",
                    ghost_eligible(&tab.path),
                    tab.buffer.selection().is_empty()
                )
            });
            self.ghost = None;
            return;
        }
        let caret = tab.buffer.selection().caret();
        let full = tab.buffer.to_string();
        let prefix = crate::ghost::cap_prefix(&full[..caret], crate::ghost::MAX_PREFIX_CHARS);
        // The suffix starts at the END of the caret's line: the rest of that
        // line is what the model is asked to produce, so it must not also be
        // handed to it as context. See `ghost::fim_suffix_start`.
        let suffix_start = crate::ghost::fim_suffix_start(&full, caret);
        let suffix =
            crate::ghost::cap_suffix(&full[suffix_start..], crate::ghost::MAX_SUFFIX_CHARS);
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
            ghost_log(|| {
                format!(
                    "cache hit at {anchor:?}: {:?}",
                    self.ghost.as_ref().map(|g| g.text.as_str())
                )
            });
            return;
        }

        // Cache miss: clear the stale suggestion and issue a debounced request.
        self.ghost = None;
        self.ghost_gen += 1;
        let generation = self.ghost_gen;
        ghost_log(|| {
            format!(
                "request gen={generation} at {anchor:?}, prefix tail {:?}",
                prefix
                    .char_indices()
                    .rev()
                    .nth(23)
                    .map_or(prefix.as_str(), |(i, _)| &prefix[i..])
            )
        });
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
            ghost_log(|| format!("drop stale gen={generation} (now {})", self.ghost_gen));
            return; // superseded
        }
        let Some(raw) = content else {
            ghost_log(|| format!("gen={generation}: no content (error/abort/timeout)"));
            self.ghost = None;
            return;
        };
        if raw.is_empty() {
            ghost_log(|| format!("gen={generation}: empty completion"));
            self.ghost = None;
            return;
        }
        self.ghost_cache.put(prefix, suffix, raw.clone());
        self.ghost = crate::ghost::post_process(&raw, &line_suffix, max_lines)
            .map(|text| GhostState { text, anchor });
        ghost_log(|| {
            format!(
                "gen={generation}: raw {raw:?} → shown {:?}",
                self.ghost.as_ref().map(|g| g.text.as_str())
            )
        });
    }

    /// Accept the ghost text (Tab): insert it at the caret.
    fn ghost_accept(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.ghost.take() else {
            return;
        };
        let record = self.with_edit(|b| b.insert_text(&g.text));
        self.after_edit(record, cx);
    }

    /// Accept only the next word of the ghost (⌥→, JetBrains FLCC's word-level
    /// partial accept): insert [`crate::ghost::first_word`] and keep the rest
    /// ghosted. The remainder is seeded into the cache under the *current*
    /// (prefix, suffix) key first, so the `after_edit` → `schedule_ghost` pass
    /// serves it back instantly as a typed-through hit instead of clearing the
    /// ghost and re-querying the model.
    fn ghost_accept_word(&mut self, cx: &mut Context<Self>) {
        let Some(g) = self.ghost.clone() else {
            return;
        };
        let word = crate::ghost::first_word(&g.text);
        if word.len() == g.text.len() {
            // Last word — same as a full accept.
            self.ghost_accept(cx);
            return;
        }
        let word = word.to_string();
        if let Some(tab) = self.editor.active_tab() {
            let caret = tab.buffer.selection().caret();
            let full = tab.buffer.to_string();
            let prefix = crate::ghost::cap_prefix(&full[..caret], crate::ghost::MAX_PREFIX_CHARS);
            let suffix = crate::ghost::cap_suffix(&full[caret..], crate::ghost::MAX_SUFFIX_CHARS);
            self.ghost_cache.put(prefix, suffix, g.text.clone());
        }
        let record = self.with_edit(|b| b.insert_text(&word));
        self.after_edit(record, cx);
    }

    // ── Hover ─────────────────────────────────────────────────────────────────

    /// Pointer moved over the code at row `row`, window x `x`: schedule a hover
    /// request after a 300ms dwell if the target cell changed.
    pub fn editor_hover_move(&mut self, row: usize, x: f32) {
        if self.editor_selecting {
            return;
        }
        // Fold in the horizontal scroll so click→column and caret geometry track
        // the glyphs after the code list scrolls sideways (the text_left canvas
        // captures a fixed left edge; the content shifts by `editor_h_scroll`).
        let text_left =
            f32::from_bits(self.editor_text_left.load(Ordering::Relaxed)) + self.editor_h_scroll();
        let cw = self.char_w();
        let (path, pos, cell) = {
            let Some(tab) = self.editor.active_tab() else {
                return;
            };
            if !lsp_eligible(&tab.path) {
                return;
            }
            let row = row.min(tab.line_count().saturating_sub(1));
            let col = editor_view::px_to_char_col(&tab.buffer.line(row), x - text_left, cw);
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
            let position = jade_lsp::Position::new(pos.line as u32, pos.character as u32);
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
        // Fold in the horizontal scroll so click→column and caret geometry track
        // the glyphs after the code list scrolls sideways (the text_left canvas
        // captures a fixed left edge; the content shifts by `editor_h_scroll`).
        let text_left =
            f32::from_bits(self.editor_text_left.load(Ordering::Relaxed)) + self.editor_h_scroll();
        let cw = self.char_w();
        let (path, pos) = {
            let Some(tab) = self.editor.active_tab() else {
                return;
            };
            if !lsp_eligible(&tab.path) {
                return;
            }
            let row = row.min(tab.line_count().saturating_sub(1));
            let col = editor_view::px_to_char_col(&tab.buffer.line(row), x - text_left, cw);
            let byte = tab.buffer.point_to_offset(Point::new(row, col));
            (tab.path.clone(), tab.buffer.offset_to_lsp(byte))
        };
        let Some(lsp) = self.lsp.clone() else {
            return;
        };
        let tx = self.app_tx.clone();
        self.runtime.spawn(async move {
            let position = jade_lsp::Position::new(pos.line as u32, pos.character as u32);
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
                        "[jade] clangd unavailable: {e}"
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
                self.status_line("[jade] clangd exited");
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
        edits: Vec<jade_lsp::Utf16RangeEdit>,
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
                self.status_line(&format!("[jade] asm failed: {e}"));
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

    /// The 1-based line the debugger is paused on, when it falls in the active
    /// file — the editor draws the execution pointer (gutter arrow + amber row
    /// wash) there. Follows the selected stack frame so clicking an outer frame
    /// moves the pointer with it; frame 0 matches the raw stop location.
    pub fn exec_pointer_line(&self) -> Option<u32> {
        if self.debug.status != crate::debug::DebugStatus::Paused {
            return None;
        }
        let active = self.active_file.as_ref()?;
        let (file, line) = match self.debug.frames.get(self.debug.active_frame) {
            Some(f) => (f.file.as_str(), f.line),
            None => {
                let (f, l) = self.debug.location.as_ref()?;
                (f.as_str(), *l)
            }
        };
        crate::debug::location_matches_file(active, file).then_some(line)
    }

    /// Re-bake the tensor-preview texture of every enabled buffer whose newest
    /// frame advanced past the cached one, and prune previews whose tensors
    /// are gone (run restart clears `training`). Runs at the top of `render()`,
    /// so the staleness check is per-repaint but the pixel work is once per
    /// NEW frame — the same cadence as the TS `drawHeatmap` on `tensorFrame`.
    /// Replaced/removed images are released via `drop_image` — the sprite
    /// atlas does NOT free textures on `RenderImage` drop, and a training run
    /// bakes many per second.
    fn ensure_preview_images(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dead: Vec<String> = self
            .preview_images
            .keys()
            .filter(|k| !self.training.tensors.contains_key(*k))
            .cloned()
            .collect();
        for k in dead {
            if let Some(old) = self.preview_images.remove(&k) {
                let _ = cx.drop_image(old.image, Some(window));
            }
        }
        for (name, ring) in &self.training.tensors {
            if !self.registry.is_enabled(Kind::Buffer, name) {
                continue;
            }
            let Some(frame) = ring.back() else { continue };
            let stale = self
                .preview_images
                .get(name)
                .map(|p| p.step != frame.step)
                .unwrap_or(true);
            if stale {
                if let Some(old) = self.preview_images.insert(
                    name.clone(),
                    crate::panels::training_view::build_preview_image(frame),
                ) {
                    let _ = cx.drop_image(old.image, Some(window));
                }
            }
        }
    }

    /// The 3D overlay's Metal renderer, created lazily on first use. `None`
    /// after a failed init (no Metal device / shader compile error) — the
    /// overlay then falls back to the CPU painter.
    #[cfg(target_os = "macos")]
    pub fn wg3d_gpu(&mut self) -> Option<&mut crate::wg3d::metal::MetalWg3d> {
        if self.wg3d_gpu.is_none() && !self.wg3d_gpu_failed {
            self.wg3d_gpu = crate::wg3d::metal::MetalWg3d::new();
            if self.wg3d_gpu.is_none() {
                self.wg3d_gpu_failed = true;
                eprintln!("[jade] wg3d Metal init failed — using the CPU painter");
            }
        }
        self.wg3d_gpu.as_mut()
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
            timer_groups: self.timer_groups.defs().to_vec(),
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
fn flatten_hover(hover: &jade_lsp::Hover) -> String {
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
        // Fold in the horizontal scroll so click→column and caret geometry track
        // the glyphs after the code list scrolls sideways (the text_left canvas
        // captures a fixed left edge; the content shifts by `editor_h_scroll`).
        let text_left =
            f32::from_bits(self.editor_text_left.load(Ordering::Relaxed)) + self.editor_h_scroll();
        let cw = self.char_w();
        // Anchor at the caret cell, using the captured text-left and the row's
        // offset from the current scroll top (best-effort IME candidate position).
        let col = editor_view::DisplayLine::new(tab.line(p.row)).display_col(p.col);
        let x = px(text_left + editor_view::col_to_px(col, cw));
        let drow = tab.display_row(p.row);
        let y = element_bounds.origin.y + px((drow.saturating_sub(top)) as f32 * LINE_H);
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

        // Re-bake tensor-preview textures whose newest frame advanced (a step
        // compare per enabled buffer; the bake itself runs once per NEW frame,
        // never per repaint — see `ensure_preview_images`).
        self.ensure_preview_images(window, cx);

        // Create the terminal (and its focus handle) on first show of the strip.
        if self.output_visible && self.bottom_view == BottomView::Terminal {
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

        // Toast sweeper (GUI only): while any build toast is up, poll a short
        // timer, drop the expired ones, and repaint so they vanish on their own.
        // Guarded like the blink driver so only one sweeper runs at a time; it
        // exits (clearing the flag) once the stack empties, and `render`
        // re-spawns it when the next toast is raised.
        if !self.toasts.is_empty() && !self.toast_sweeping {
            self.toast_sweeping = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                    let keep = this.update(cx, |app, cx| {
                        let now = app.now_ms();
                        let before = app.toasts.len();
                        app.toasts
                            .retain(|t| now.saturating_sub(t.created_ms) < TOAST_MS);
                        if app.toasts.len() != before {
                            cx.notify();
                        }
                        if app.toasts.is_empty() {
                            app.toast_sweeping = false;
                            false // done — let the task end
                        } else {
                            true
                        }
                    });
                    match keep {
                        Ok(true) => continue,
                        Ok(false) | Err(_) => break, // empty, or window closed
                    }
                }
            })
            .detach();
        }

        // A file was just opened: hand the editor keyboard focus (open sites have
        // no Window; this render does). Skipped while an overlay owns input.
        if self.pending_editor_focus {
            self.pending_editor_focus = false;
            if self.quick_open.is_none() && self.find.is_none() {
                if let Some(h) = &self.editor_focus {
                    h.focus(window, cx);
                }
            }
        }

        // Find-bar focus (⌘F/Ctrl+F): hand the captured-keystroke buffer focus
        // once when the bar opens. Focusing only on the open transition (not every
        // frame) lets the user click into the editor with the bar still showing.
        if self.pending_find_focus && self.find.is_some() {
            self.pending_find_focus = false;
            let fh = self.find_focus.get_or_insert_with(|| cx.focus_handle()).clone();
            if !fh.is_focused(window) {
                fh.focus(window, cx);
            }
        }
        // Sample whether the bar owns focus so the (window-less) renderer knows
        // when to blink the field caret vs. leave it dim while you edit code.
        self.find_bar_focused = self
            .find
            .is_some()
            .then(|| self.find_focus.as_ref().map(|f| f.is_focused(window)).unwrap_or(false))
            .unwrap_or(false);

        // Benchmark-name input focus (§5.4): focus it while naming so keystrokes
        // reach the inline input instead of the editor/terminal. Skipped while a
        // dim editor (below) also wants focus — the two rarely coexist, but
        // guard the ping-pong anyway.
        let bench_handle = self.bench_focus.get_or_insert_with(|| cx.focus_handle()).clone();
        if self.bench_naming.is_some() && !bench_handle.is_focused(window) && self.dim_edit.is_none()
        {
            bench_handle.focus(window, cx);
        }

        // Rows×cols dim-editor focus (§7.2/§5.6): keep keystrokes routed to the
        // inline editor (wg3d toolbar or telemetry sidebar row) while a session
        // is open, same pattern as the benchmark-name input above.
        if self.dim_edit.is_some() {
            let dim_handle = self.dim_edit_focus.get_or_insert_with(|| cx.focus_handle()).clone();
            if !dim_handle.is_focused(window) && self.bench_naming.is_none() {
                dim_handle.focus(window, cx);
            }
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
                    // ⌘Q. The menu item alone does NOT give you this: gpui builds
                    // the macOS menu itself and takes each item's key equivalent
                    // from a keymap binding, so with no keymap the Quit item
                    // carried no shortcut and the keystroke matched nothing —
                    // it arrived here and fell through to `_ => {}`. Handled on
                    // the same root path as ⌘P and ⌘O, which are known to work.
                    // Quitting runs `on_app_quit`, which stops llama-server.
                    "q" if m.platform => {
                        cx.quit();
                    }
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
            // Bottom-panel resize drag: while the top-edge handle is held, track
            // the pointer at the root so an upward drag past the panel edge (over
            // the editor) still resizes. Dragging up grows the panel.
            .on_mouse_move(cx.listener(|app: &mut JadeApp, ev: &gpui::MouseMoveEvent, window, cx| {
                let Some((start_y, start_h)) = app.bottom_resize else { return };
                if ev.pressed_button != Some(MouseButton::Left) {
                    app.bottom_resize = None;
                    return;
                }
                let dy = start_y - f32::from(ev.position.y);
                let max_h = (f32::from(window.viewport_size().height) - 200.).max(160.);
                app.bottom_height = (start_h + dy).clamp(120., max_h);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|app: &mut JadeApp, _ev: &gpui::MouseUpEvent, _w, cx| {
                    if app.bottom_resize.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(action_bar(self, cx, &theme))
            .child(project_tabs(self, cx, &theme))
            .child(
                // Main area: left panel | center content | right runtime sidebar.
                // min_h(0): flex children default to min-height:auto, so tall
                // panel content (structure outline, telemetry lists) would grow
                // the row past the viewport and push the terminal off-screen.
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
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
        } else if self.output_visible {
            // Vertical drawer slide, mirroring the sidebar: the strip's inner
            // card keeps its fixed height; the wrapper animates and clips.
            use gpui::{Animation, AnimationExt as _};
            let closing = self.bottom_closing;
            let full = self.bottom_height + 6.0; // card height + 6px top gutter
            root = root.child(
                div()
                    .flex_none()
                    .overflow_hidden()
                    .child(bottom_panel(self, cx, &theme, term_handle))
                    .with_animation(
                        ("bottom-slide", self.bottom_anim_gen),
                        Animation::new(std::time::Duration::from_millis(SIDEBAR_SLIDE_MS))
                            .with_easing(gpui::ease_out_quint()),
                        move |el, t| {
                            let h = if closing { full * (1.0 - t) } else { full * t };
                            el.h(px(h))
                        },
                    ),
            );
        }
        let mut root = root
            .child(memory_bar(self, &theme))
            .child(status_strip(self, &theme));

        // §7.2 open/close hook: while visible, overlay the full-window 3D grid
        // on top of everything and hand it keyboard focus (for Esc).
        if self.wg3d.visible {
            let focus = crate::wg3d::render::ensure_focus(self, cx);
            // Don't steal focus back from the toolbar's dim editor (below).
            if !focus.is_focused(window) && self.dim_edit.is_none() {
                focus.focus(window, cx);
            }
            let vp = window.viewport_size();
            let scale = window.scale_factor();
            let overlay = crate::wg3d::render::overlay(
                self,
                focus,
                f32::from(vp.width),
                f32::from(vp.height),
                scale,
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

        // Pre-run tracking panel: pick timers/buffers before Run/Debug launches.
        // Focused so Esc/Enter land on the overlay, not the editor.
        if self.pre_run.is_some() {
            let focus = self
                .pre_run_focus
                .get_or_insert_with(|| cx.focus_handle())
                .clone();
            if !focus.is_focused(window) {
                focus.focus(window, cx);
            }
            root = root.child(crate::panels::pre_run::overlay(self, focus, cx));
        }

        // AI settings menu: a top-right popover under the sparkle button
        // (completion · multiline · model tier + live status).
        if self.ai_menu_open {
            root = root.child(ai_menu(self, cx, &theme));
        }

        // Build toasts: a bottom-right stack floating over everything. Each
        // self-expires (the sweeper above), fading out over its last ~600ms.
        if !self.toasts.is_empty() {
            root = root.child(toast_overlay(self, &theme));
        }
        root
    }
}

/// The card elevation, applied to the sidebar / editor / runtime / terminal
/// cards so both themes share one step of lift. Kumo puts `shadow-xs` on a `LayerCard` and `shadow-sm`
/// on anything that floats over the page, which is what these cards do.
fn card_shadow() -> Vec<BoxShadow> {
    kumo::shadow_sm()
}

/// CLion-style project subtabs: one chip per opened project, active follows
/// `workspace_root`. Hidden until a second project is opened. Switching goes
/// through `open_project`, whose per-workspace persistence restores each
/// project's tabs/breakpoints.
fn project_tabs(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> gpui::AnyElement {
    if app.open_projects.len() < 2 {
        return div().into_any_element();
    }
    let t = &theme.kumo;
    let bar = TabBar::new(TabsAppearance::Segmented).size(KumoSize::Sm);
    let mut tabs = TabBar::new(TabsAppearance::Segmented).size(KumoSize::Sm);

    for (i, dir) in app.open_projects.iter().enumerate() {
        let active = *dir == app.workspace_root;
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());
        let target = dir.clone();
        // Close (×): removes this project from the subtabs; closing the active
        // one switches to a neighbor (see `close_project`). `stop_propagation`
        // keeps an inactive tab's switch-on-click from firing before the close.
        let close_target = dir.clone();
        let close = div()
            .id(("proj-tab-close", i))
            .px(scale::SPACE_1)
            .cursor_pointer()
            .on_click(cx.listener(move |a: &mut JadeApp, _ev, _w, cx| {
                cx.stop_propagation();
                a.close_project(&close_target);
                cx.notify();
            }))
            .child(kumo::icon("x", 11., t.text_subtle))
            .into_any_element();

        let trigger = bar
            .trigger(
                TabItem::new(("proj-tab", i), name, active)
                    .icon("folder")
                    .trailing(close),
                t,
            )
            .on_click(cx.listener(move |a: &mut JadeApp, _ev, _w, cx| {
                a.open_project(target.clone());
                cx.notify();
            }));
        tabs = tabs.push(trigger);
    }

    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(32.))
        .px(scale::SPACE_3)
        .bg(t.elevated)
        .border_b_1()
        .border_color(t.hairline)
        .child(tabs.render(t))
        .into_any_element()
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
                a.action_toggle_output(cx);
            } else {
                a.set_bottom_view(BottomView::Terminal);
            }
            a.schedule_ui_save(cx); // terminalVisible (§1.2)
        }))
        .child(icon_btn("tgl-flow", "corner-down-right", theme, app.flow_visible, cx, |a, _| {
            a.action_toggle_flow()
        }))
        .child(icon_btn("tgl-runtime", "gauge", theme, app.runtime_visible, cx, |a, cx| {
            a.action_toggle_runtime(cx)
        }));

    // Diagnostic pills (always visible, zero included — screenshot center-left).
    let (errs, warns, infos) = app.active_diag_counts();
    let diag_badges = div()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .child(diag_pill("circle-x", errs, BadgeVariant::Error, theme))
        .child(diag_pill("triangle-alert", warns, BadgeVariant::Warning, theme))
        .child(diag_pill("info", infos, BadgeVariant::Secondary, theme));

    let right_group = div()
        .flex()
        .items_center()
        .gap(scale::SPACE_2)
        // ASM viewer (§6, ⌘⇧A): plain icon+label, no box.
        .child(flat_btn("chip-asm", "code", "ASM", theme.muted, theme, app.asm_visible, false, cx, |a, cx| {
            a.toggle_asm(cx)
        }))
        // Hand-off to CLion at the caret line (⌘⇧C): Jade analyzes, CLion edits.
        .child(flat_btn(
            "chip-clion",
            "external-link",
            "CLion",
            theme.muted,
            theme,
            false,
            app.editor.active_tab().is_none(),
            cx,
            |a, _| a.action_open_in_clion(),
        ))
        // Build / Run: outlined accent pills (screenshot ref).
        .child(pill_btn(
            "btn-build",
            "hammer",
            build_label,
            ButtonVariant::Secondary,
            theme,
            app.building,
            cx,
            |a, _| a.action_build(),
        ))
        .child(pill_btn(
            "btn-run",
            "play",
            run_label,
            ButtonVariant::Primary,
            theme,
            app.running || !can_run,
            cx,
            |a, _| a.action_run(),
        ))
        .child(flat_btn("btn-debug", "bug", "Debug", theme.amber, theme, app.debugging, app.building, cx, |a, _| {
            a.action_debug()
        }))
        .child(flat_btn("btn-stop", "square", "Stop", theme.red, theme, false, false, cx, |a, _| {
            a.action_stop()
        }))
        // Trailing icon-only cluster: AI settings menu · theme · open-folder.
        // The former standalone AI toggles (ghost/eye, multiline/layers) plus the
        // model selector now live in the sparkle popover (see `ai_menu`), matching
        // the old Jade's single AI menu.
        .child(icon_btn("btn-ai", "sparkles", theme, ai_active || app.ai_menu_open, cx, |a, _| {
            a.toggle_ai_menu()
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

    // The bar rides on the elevated layer with a single Kumo hairline under it
    // and no other chrome, so the controls are the only marks on the strip.
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(44.))
        .pl(px(80.)) // clear the traffic lights (hiddenInset title bar)
        .pr(scale::SPACE_3)
        .bg(theme.kumo.elevated)
        .border_b_1()
        .border_color(theme.kumo.hairline)
        // Window-drag region: empty parts of the bar drag the window; the
        // buttons' own click hitboxes take priority (`no-drag` equivalent).
        .window_control_area(WindowControlArea::Drag)
        .child(
            div()
                .flex()
                .items_center()
                .gap(scale::SPACE_3)
                .child(toggles)
                // A Kumo hairline rule between the view toggles and the
                // diagnostics, so the bar reads as clusters and not one run of
                // controls.
                .child(separator_v(&theme.kumo, 16.))
                .child(diag_badges),
        )
        .child(right_group)
}

/// Build-result **toast stack**: a bottom-right column of self-expiring cards
/// (see [`Toast`] / [`JadeApp::push_toast`]). Success wears the emerald accent +
/// check glyph, failure the red + circle-x. Each card fades in on appearance and
/// fades out over its final ~600ms; the sweeper in `render` removes it after
/// [`TOAST_MS`]. Non-interactive (`occlude` off) so it never eats editor clicks.
fn toast_overlay(app: &JadeApp, theme: &Theme) -> gpui::AnyElement {
    use gpui::{Animation, AnimationExt as _};
    let now = app.now_ms();
    let mut col = div()
        .absolute()
        .bottom(px(52.)) // clear the status strip + memory bar
        .right(px(16.))
        .flex()
        .flex_col()
        .items_end()
        .gap_2();
    for t in &app.toasts {
        let (accent, icon) = match t.kind {
            ToastKind::Success => (theme.kumo.success, "circle-check"),
            ToastKind::Error => (theme.kumo.danger, "circle-x"),
        };
        let age = now.saturating_sub(t.created_ms);
        // Fade the whole card out over its last stretch; the entrance animation
        // below drives the fade-in during the first ~180ms.
        let fade = if age + 600 >= TOAST_MS {
            (TOAST_MS.saturating_sub(age) as f32 / 600.0).clamp(0.0, 1.0)
        } else {
            1.0
        };
        // A Kumo Card (`LAYER_CARD_SURFACE_CLASSES`) floated over the editor.
        let card = Card::new(&theme.kumo)
            .flex()
            .flex_row()
            .items_center()
            .gap(scale::SPACE_2_5)
            .pl(scale::SPACE_3)
            .pr(scale::SPACE_4)
            .py(scale::SPACE_2_5)
            .min_w(px(210.))
            .max_w(px(340.))
            .shadow(kumo::shadow_md())
            .text_size(scale::TEXT_SM)
            .text_color(theme.kumo.text_default)
            // Colored accent pill on the leading edge (kind at a glance).
            .child(div().w(px(3.)).h(px(20.)).rounded_full().bg(accent))
            .child(kumo::icon(icon, 15., accent))
            .child(div().child(t.message.clone()))
            .opacity(fade);
        col = col.child(card.with_animation(
            ("toast", t.created_ms as usize),
            Animation::new(std::time::Duration::from_millis(180))
                .with_easing(gpui::ease_out_quint()),
            move |el, p| el.opacity(fade * p),
        ));
    }
    col.into_any_element()
}

/// The sparkle **AI settings menu** (old-Jade parity, `app.ts:532-586`): a
/// top-right popover under the sparkle button holding the AI-completion and
/// multi-line toggles, the managed-model tier selector, and a live status
/// footer. A full-window transparent backdrop closes it on any outside click;
/// clicks inside are swallowed so toggling a row doesn't dismiss the menu.
fn ai_menu(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> gpui::AnyElement {
    let enabled = app.ai_completion_enabled;
    let multiline = app.ai_multiline;
    let selected_model = app.ai_model;
    // The tier only applies to the server Jade manages itself — an adopted
    // external endpoint picks its own model (`app.ts:546-547`).
    let external = matches!(app.ai_status.state, AiState::Ready)
        && app.ai_status.detail.starts_with("Connected to");

    // A 14px checkbox: accent-filled when on, hollow-bordered when off.
    let checkbox = |on: bool| {
        let mut b = div()
            .w(px(14.))
            .h(px(14.))
            .flex_none()
            .rounded_sm()
            .border_1()
            .border_color(rgb(if on { theme.accent } else { theme.muted }));
        if on {
            b = b.bg(rgb(theme.accent));
        }
        b
    };
    // A 12px radio dot: accent-filled when selected, hollow otherwise.
    let radio = |on: bool| {
        let mut d = div()
            .w(px(12.))
            .h(px(12.))
            .flex_none()
            .rounded_full()
            .border_1()
            .border_color(rgb(if on { theme.accent } else { theme.muted }));
        if on {
            d = d.bg(rgb(theme.accent));
        }
        d
    };

    // Toggle rows.
    let hover_bg = theme.border;
    let row_enabled = div()
        .id("ai-opt-enabled")
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_1()
        .py(px(4.))
        .rounded_sm()
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(hover_bg)))
        .on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
            a.action_toggle_ai_completion();
            a.schedule_ui_save(cx); // aiCompletionEnabled (§1.2)
            cx.notify();
        }))
        .child(checkbox(enabled))
        .child(div().text_color(rgb(theme.text)).child("AI completion"));

    let row_multiline = div()
        .id("ai-opt-multiline")
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_1()
        .py(px(4.))
        .rounded_sm()
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(hover_bg)))
        .on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
            a.action_toggle_ai_multiline();
            cx.notify();
        }))
        .child(checkbox(multiline))
        .child(
            div()
                .text_color(rgb(theme.text))
                .child("Multi-line suggestions"),
        );

    // Model tier rows (radio-style; disabled/greyed when an external server owns
    // the model choice).
    let model_opts = [
        (AiModelId::Sprite, "sprite-100m"),
        (AiModelId::Fastest, "Fastest — Qwen2.5-Coder 0.5B"),
        (AiModelId::Fast, "Fast — Qwen2.5-Coder 1.5B"),
        (AiModelId::Balanced, "Balanced — 3B (~3.3 GB)"),
        (AiModelId::Best, "Best — 7B (~8 GB)"),
    ];
    let mut model_list = div().flex().flex_col().gap(px(2.));
    for (i, (id, label)) in model_opts.into_iter().enumerate() {
        let sel = id == selected_model;
        let mut row = div()
            .id(("ai-model", i))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_1()
            .py(px(4.))
            .rounded_sm()
            .child(radio(sel))
            .child(
                div()
                    .flex_1()
                    .text_color(rgb(if external {
                        theme.muted
                    } else if sel {
                        theme.text
                    } else {
                        theme.muted
                    }))
                    .child(label),
            );
        if !external {
            row = row
                .cursor_pointer()
                .hover(move |s| s.bg(rgb(hover_bg)))
                .on_click(cx.listener(move |a: &mut JadeApp, _e, _w, cx| {
                    a.set_ai_model(id);
                    cx.notify();
                }));
        }
        model_list = model_list.child(row);
    }

    let divider = || div().h(px(1.)).bg(rgb(theme.border)).my(px(2.));
    let state_label = match app.ai_status.state {
        AiState::Disabled => "off",
        AiState::Starting => "starting",
        AiState::Ready => "ready",
        AiState::Error => "error",
    };
    let status = if app.ai_status.detail.is_empty() {
        state_label.to_string()
    } else {
        format!("{state_label} — {}", app.ai_status.detail)
    };

    let panel = div()
        .id("ai-menu")
        .absolute()
        .top(px(46.))
        .right(px(12.))
        .w(px(268.))
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .bg(rgb(theme.panel))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_lg()
        .shadow(card_shadow())
        .text_xs()
        // Swallow inside-clicks so a row toggle doesn't hit the backdrop
        // (`app.ts:551` `ev.stopPropagation()`).
        .on_click(cx.listener(|_a: &mut JadeApp, _e, _w, cx| cx.stop_propagation()))
        .child(row_enabled)
        .child(row_multiline)
        .child(divider())
        .child(div().text_color(rgb(theme.muted)).child("MODEL"))
        .child(model_list)
        .child(divider())
        .child(div().text_color(rgb(theme.muted)).child(status));

    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .child(
            div()
                .id("ai-menu-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_click(cx.listener(|a: &mut JadeApp, _e, _w, cx| {
                    a.close_ai_menu();
                    cx.notify();
                })),
        )
        .child(panel)
        .into_any_element()
}

/// An icon-only action-bar button (no box, no label): muted at rest, accent
/// when `active`, hover wash. 26px square hit target around a 15px glyph.
/// An icon-only toggle. This is Kumo's `<Button shape="square" size="sm"
/// variant="ghost">` — a 26px square (`h-6.5`) that takes the brand ink while
/// the tool it toggles is on.
fn icon_btn(
    id: &'static str,
    icon: &'static str,
    theme: &Theme,
    active: bool,
    cx: &mut Context<JadeApp>,
    f: impl Fn(&mut JadeApp, &mut Context<JadeApp>) + 'static,
) -> impl IntoElement {
    kumo::button::icon_button(id, icon, active, &theme.kumo).on_click(cx.listener(
        move |a, _ev, _win, cx| {
            f(a, cx);
            cx.notify();
        },
    ))
}

/// A flat icon+label button — Kumo's `variant="ghost"` at `size="sm"`. `base` is
/// the resting ink (Debug amber, Stop red, ASM subtle); an active button takes
/// the brand ink and a disabled one drops to `text-kumo-subtle`.
#[allow(clippy::too_many_arguments)]
fn flat_btn(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    base: u32,
    theme: &Theme,
    active: bool,
    disabled: bool,
    cx: &mut Context<JadeApp>,
    f: impl Fn(&mut JadeApp, &mut Context<JadeApp>) + 'static,
) -> impl IntoElement {
    let ink = if disabled {
        theme.kumo.text_subtle
    } else if active {
        theme.kumo.brand
    } else {
        rgb(base)
    };
    let el = Button::new(id, label)
        .variant(ButtonVariant::Ghost)
        .size(KumoSize::Sm)
        .icon(icon)
        .ink(ink)
        .disabled(disabled)
        .render(&theme.kumo);
    if disabled {
        el
    } else {
        el.on_click(cx.listener(move |a, _ev, _win, cx| {
            f(a, cx);
            cx.notify();
        }))
    }
}

/// The Build / Run pair. Kumo's `variant="primary"` is the one solid fill on
/// the bar — exactly one committing action per screen — and Build sits beside
/// it as `variant="secondary"`.
#[allow(clippy::too_many_arguments)]
fn pill_btn(
    id: &'static str,
    icon: &'static str,
    label: String,
    variant: ButtonVariant,
    theme: &Theme,
    busy: bool,
    cx: &mut Context<JadeApp>,
    f: impl Fn(&mut JadeApp, &mut Context<JadeApp>) + 'static,
) -> impl IntoElement {
    let el = Button::new(id, label)
        .variant(variant)
        .size(KumoSize::Sm)
        .icon(icon)
        .disabled(busy)
        .render(&theme.kumo);
    if busy {
        el
    } else {
        el.on_click(cx.listener(move |a, _ev, _win, cx| {
            f(a, cx);
            cx.notify();
        }))
    }
}

/// One always-visible diagnostic count — a Kumo Badge on the matching status
/// tint. The counts show even at zero, so the bar does not reflow as
/// diagnostics arrive.
fn diag_pill(icon: &'static str, count: usize, variant: BadgeVariant, theme: &Theme) -> impl IntoElement {
    Badge::new(format!("{count}"))
        .variant(variant)
        .icon(icon)
        .tabular(true)
        .render(&theme.kumo)
}

/// Left panel: the file-tree card (deliverable §2, floating-card look §2). When
/// collapsed it shrinks to a 28px clickable strip showing a stacked "FILES" label
/// that reopens it (app.ts:449-459, main.css:481-495).
fn left_panel(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    use gpui::{Animation, AnimationExt as _};

    // Width slide between the expanded card (260) and the collapsed strip (28),
    // same 180ms drawer feel as the other two regions. Content swaps instantly;
    // only the width animates (clipped by overflow_hidden on both branches).
    let (from, to) = if app.sidebar_collapsed {
        (260.0_f32, 28.0_f32)
    } else {
        (28.0_f32, 260.0_f32)
    };
    let inner = left_panel_inner(app, cx, theme);
    return div()
        .flex()
        .flex_none()
        .overflow_hidden()
        .child(inner)
        .with_animation(
            ("left-slide", app.left_anim_gen),
            Animation::new(std::time::Duration::from_millis(SIDEBAR_SLIDE_MS))
                .with_easing(gpui::ease_out_quint()),
            move |el, t| el.w(px(from + (to - from) * t)),
        )
        .into_any_element();
}

fn left_panel_inner(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
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
        return Card::new(&theme.kumo)
            .id("sidebar-collapsed")
            .flex()
            .flex_col()
            .items_center()
            .w(px(28.))
            .h_full()
            .flex_none()
            .bg(theme.kumo.elevated)
            .cursor_pointer()
            .hover(|s| s.bg(theme.kumo.tint))
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
    // A Kumo Card on the elevated layer — the same shell every floating region
    // in the window now wears.
    Card::new(&theme.kumo)
        .flex()
        .flex_col()
        .flex_none()
        .gap(scale::SPACE_2)
        .w(px(260.))
        .h_full()
        .p(scale::SPACE_2_5)
        .bg(theme.kumo.elevated)
        .child(structure_panel::tab_switcher(app, cx, theme))
        .child(
            // The tree/outline scrolls inside the card (min_h(0) so the flex
            // child can shrink instead of growing the card past the row).
            div()
                .id("left-panel-body")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .child(body),
        )
        .into_any_element()
}

/// Center: the tab strip + read-only code viewer (deliverables §3, §5), replacing
/// the placeholder. With no workspace open (§2) the welcome overlay covers this
/// area instead.
fn center_content(app: &JadeApp, cx: &mut Context<JadeApp>, theme: &Theme) -> impl IntoElement {
    // The editor sits on the canvas, not on `base` — the code is the content,
    // so it takes the darkest surface and the chrome lifts off it.
    let base = Card::new(&theme.kumo)
        .relative()
        .flex()
        .flex_col()
        .flex_1()
        .bg(theme.kumo.canvas);

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
    let t = &theme.kumo;
    let hint = |s: &str| {
        KumoText::new(s.to_string())
            .tone(TextTone::Secondary)
            .size(KumoSize::Xs)
            .render(t)
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
                // welcome-title — Kumo `heading1` (`text-3xl font-semibold`).
                .child(Heading::new(HeadingLevel::One, "Jade").render(t))
                // welcome-subtitle
                .child(
                    div().mt(scale::SPACE_2).child(
                        KumoText::new("Open a folder to get started")
                            .tone(TextTone::Secondary)
                            .size(KumoSize::Base)
                            .render(t),
                    ),
                )
                // The one primary action on the screen, so it takes the brand
                // fill — Kumo `variant="primary" size="lg"`.
                .child(
                    div().mt(scale::SPACE_6).child(
                        Button::new("open-folder-btn", "Open Folder")
                            .variant(ButtonVariant::Primary)
                            .size(KumoSize::Lg)
                            .icon("folder-open")
                            .render(t)
                            .on_click(cx.listener(|a: &mut JadeApp, _ev, _win, cx| {
                                a.prompt_open_project(cx);
                            })),
                    ),
                )
                // welcome-shortcuts hint row.
                .child(
                    div()
                        .mt(px(32.))
                        .flex()
                        .gap(px(20.))
                        .child(hint("⌘B File tree"))
                        .child(hint("⌘` Terminal"))
                        .child(hint("⌘E Flow arrows"))
                        .child(hint("⌘S Save")),
                ),
        )
}

/// Sidebar slide duration (open and close).
const SIDEBAR_SLIDE_MS: u64 = 180;

/// The right sidebar (gauge toggle): RUNTIME graphs over TRAINING + TELEMETRY,
/// sliding in/out as a drawer (§5.4; user spec 2026-07-15). The inner card
/// keeps its fixed 280px width so content doesn't reflow mid-slide; the outer
/// wrapper animates its width and clips.
fn runtime_sidebar(
    app: &JadeApp,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
    bench_handle: FocusHandle,
) -> gpui::AnyElement {
    use gpui::{Animation, AnimationExt as _};

    if !app.runtime_visible {
        return div().into_any_element();
    }
    let card = Card::new(&theme.kumo)
        .id("runtime-sidebar")
        .flex()
        .flex_none()
        .flex_col()
        .gap(scale::SPACE_3)
        .w(px(280.))
        .h_full()
        .min_h(px(0.))
        .p(scale::SPACE_2_5)
        .bg(theme.kumo.elevated)
        .overflow_y_scroll()
        .child(runtime_panel::render(app, bench_handle, cx))
        .child(training_view::render(app, cx))
        .child(telemetry_sidebar::render(app, cx));

    let closing = app.sidebar_closing;
    div()
        .flex()
        .flex_none()
        .overflow_hidden()
        .child(card)
        .with_animation(
            ("sidebar-slide", app.sidebar_anim_gen),
            Animation::new(std::time::Duration::from_millis(SIDEBAR_SLIDE_MS))
                .with_easing(gpui::ease_out_quint()),
            move |el, t| {
                let w = if closing { 280.0 * (1.0 - t) } else { 280.0 * t };
                el.w(px(w))
            },
        )
        .into_any_element()
}

/// Bottom panel (§5.2): a header (view toggle · new-terminal · minimize) over the
/// live TERMINAL grid or the OUTPUT scrollback fallback. `[jade]`/build/run
/// status lines land in OUTPUT (the terminal is a real shell we can't inject
/// display text into — see `terminal_panel`).
fn bottom_panel(
    app: &JadeApp,
    cx: &mut Context<JadeApp>,
    theme: &Theme,
    term_handle: FocusHandle,
) -> impl IntoElement {
    let is_term = app.bottom_view == BottomView::Terminal;

    // View-toggle tabs: TERMINAL | OUTPUT. A Kumo segmented Tabs at `size="sm"`
    // — a raised pill riding in a recessed trough.
    let bar = TabBar::new(TabsAppearance::Segmented).size(KumoSize::Sm);
    let view_tab = |id: &'static str, label: &'static str, active: bool, view: BottomView| {
        bar.trigger(TabItem::new(id, label, active), &theme.kumo).on_click(
            cx.listener(move |a: &mut JadeApp, _ev, _win, cx| {
                a.set_bottom_view(view);
                cx.notify();
            }),
        )
    };

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(34.))
        .px(scale::SPACE_2)
        .border_b_1()
        .border_color(theme.kumo.hairline)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(scale::SPACE_2)
                .child(kumo::icon("terminal", 13., theme.kumo.text_subtle))
                .child(
                    TabBar::new(TabsAppearance::Segmented)
                        .size(KumoSize::Sm)
                        .push(view_tab("bv-terminal", "Terminal", is_term, BottomView::Terminal))
                        .push(view_tab("bv-output", "Output", !is_term, BottomView::Output))
                        .render(&theme.kumo),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                // New-terminal.
                .child(kumo::button::icon_button("term-new", "plus", false, &theme.kumo).on_click(
                    cx.listener(|a: &mut JadeApp, _ev, _win, cx| {
                        a.action_new_terminal();
                        cx.notify();
                    }),
                ))
                // Minimize (hide the strip).
                .child(kumo::button::icon_button("term-min", "minus", false, &theme.kumo).on_click(
                    cx.listener(|a: &mut JadeApp, _ev, _win, cx| {
                        a.action_toggle_output(cx);
                        cx.notify();
                    }),
                )),
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

    // Thin top-edge grab handle: mouse-down anchors the resize drag (the root's
    // move/up handlers do the tracking). A row-resize cursor advertises it.
    let resize_handle = div()
        .id("bottom-resize")
        .h(px(6.))
        .w_full()
        .cursor(gpui::CursorStyle::ResizeUpDown)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|a: &mut JadeApp, ev: &MouseDownEvent, _w, cx| {
                a.bottom_resize = Some((f32::from(ev.position.y), a.bottom_height));
                cx.notify();
            }),
        );

    // Floating-card treatment (§2; main.css `#terminal-area` padding 6px 6px 0 6px
    // + `.terminal-panel` card): the strip sits in a 6px gutter as a rounded,
    // bordered, shadowed card matching the sidebar / editor / runtime cards. The
    // 6px top gutter doubles as the resize grab strip.
    div()
        .flex()
        .flex_col()
        .flex_none()
        .px(px(6.))
        .child(resize_handle)
        .child(
            Card::new(&theme.kumo)
                .id("bottom-panel")
                .flex()
                .flex_col()
                .h(px(app.bottom_height))
                .w_full()
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
    // Sticky-bottom autoscroll: while armed, pin to the bottom every frame so
    // appended lines stay in view (`scroll_to_bottom` is applied in prepaint,
    // after layout, so it lands on the post-append content height). A wheel
    // scroll re-evaluates stickiness *after* the div has applied the scroll
    // (defer_in), releasing when the user moves up and re-arming at the bottom.
    if app.output_stick {
        app.output_scroll.scroll_to_bottom();
    }
    let scroll = app.output_scroll.clone();
    div()
        .id("output-panel")
        .flex_1()
        .w_full()
        .p(px(8.))
        .overflow_y_scroll()
        .track_scroll(&app.output_scroll)
        .on_scroll_wheel(cx.listener(move |_app, _ev, window, cx| {
            let scroll = scroll.clone();
            cx.defer_in(window, move |app: &mut JadeApp, _w, cx| {
                // Offset grows negative scrolling down; bottom is -max_offset.
                let from_bottom = scroll.max_offset().y + scroll.offset().y;
                let at_bottom = from_bottom <= px(4.);
                if app.output_stick != at_bottom {
                    app.output_stick = at_bottom;
                    cx.notify();
                }
            });
        }))
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
        .h(px(26.))
        .px(scale::SPACE_3)
        .bg(theme.kumo.elevated)
        .border_t_1()
        .border_color(theme.kumo.hairline)
        .child(
            div()
                .flex()
                .items_center()
                .gap(scale::SPACE_4)
                .child(metric("Sys mem", v.sys_mem, Level::Normal, theme))
                .child(metric("Heap", v.heap, v.heap_level, theme))
                .child(metric("Peak", v.peak, v.peak_level, theme))
                .child(metric("Pressure", v.pressure_dots, v.pressure_level, theme)),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(scale::SPACE_4)
                .child(metric("CPU", v.cpu, v.cpu_level, theme))
                .child(metric("GPU", v.gpu, v.gpu_level, theme)),
        )
}

/// One label/value pair on the memory bar. The value is monospaced, because a
/// number that changes every frame must not shift the label beside it.
fn metric(label: &str, value: String, level: Level, theme: &Theme) -> impl IntoElement {
    let vc = match level {
        Level::Normal => theme.kumo.text_default,
        Level::Warn => theme.kumo.text_warning,
        Level::Danger => theme.kumo.text_danger,
    };
    div()
        .flex()
        .items_center()
        .gap(scale::SPACE_1)
        .text_size(scale::TEXT_XS)
        .child(
            KumoText::new(label.to_string())
                .tone(TextTone::Secondary)
                .size(KumoSize::Xs)
                .render(&theme.kumo),
        )
        .child(
            div()
                .font_family("JetBrains Mono")
                .text_color(vc)
                .child(value),
        )
}

fn status_strip(app: &JadeApp, theme: &Theme) -> impl IntoElement {
    // A Kumo dot Badge leads the strip: green once telemetry has arrived, a
    // neutral dot while the socket is still quiet. One glance answers "is the
    // probe talking to me", which is what the strip is for.
    let live = app.scalars_seen + app.timings_seen + app.tensors_seen > 0;
    let status = Badge::new(if live { "Live" } else { "Idle" })
        .dot(if live {
            DotColor::Success
        } else {
            DotColor::Neutral
        })
        .render(&theme.kumo);

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
        .gap(scale::SPACE_2_5)
        .h(px(26.))
        .px(scale::SPACE_3)
        .bg(theme.kumo.elevated)
        .border_t_1()
        .border_color(theme.kumo.hairline)
        .child(status)
        .child(
            KumoText::new(text)
                .tone(TextTone::Secondary)
                .size(KumoSize::Xs)
                .render(&theme.kumo),
        )
}

#[cfg(test)]
mod discovery_watchdog_tests {
    use super::{discovery_should_stop, DISCOVERY_HARD_CAP_SECS, DISCOVERY_SECS};
    use std::time::Duration;

    fn s(v: u64) -> Duration {
        Duration::from_secs(v)
    }

    #[test]
    fn uninstrumented_app_keeps_the_old_fixed_window() {
        assert!(!discovery_should_stop(s(DISCOVERY_SECS - 1), None, false));
        assert!(discovery_should_stop(s(DISCOVERY_SECS), None, false));
    }

    #[test]
    fn slow_startup_waits_while_decls_prove_the_probe_is_alive() {
        // The metalLLM case: buffers declared at ~1s, then a ~25s Metal PSO
        // compile freeze before the first command buffer. A fixed 5s window
        // reported "0 timers, 20 buffers"; the scan must keep waiting.
        assert!(!discovery_should_stop(s(30), None, true));
        assert!(!discovery_should_stop(s(DISCOVERY_HARD_CAP_SECS - 1), None, true));
        assert!(discovery_should_stop(s(DISCOVERY_HARD_CAP_SECS), None, true));
    }

    #[test]
    fn warm_app_gets_a_full_post_warm_window() {
        // Warmed at 26s: the scan still runs DISCOVERY_SECS past that point.
        assert!(!discovery_should_stop(s(28), Some(s(2)), true));
        assert!(discovery_should_stop(s(26 + DISCOVERY_SECS), Some(s(DISCOVERY_SECS)), true));
        // Fast app (warm at 2s) ends on the same post-warm schedule.
        assert!(discovery_should_stop(s(2 + DISCOVERY_SECS), Some(s(DISCOVERY_SECS)), true));
    }
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
        assert_eq!(parse_jump_target("[jade] Build failed (5 error(s))"), None);
        assert_eq!(parse_jump_target("main.cpp:1:1: error: relative path"), None);
        assert_eq!(parse_jump_target("/Users/x/notes.txt: no numbers"), None);
        assert_eq!(parse_jump_target("/Users/x/a.cpp:0:1: zero line"), None);
    }
}

/// The cwd rules for a new terminal (§5.2).
#[cfg(test)]
mod terminal_cwd_tests {
    use super::terminal_cwd_for;
    use std::path::{Path, PathBuf};

    /// A real directory tree: `<tmp>/proj/src/main.cpp`.
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn fixture(tag: &str) -> Fixture {
        let base = std::env::temp_dir().join(format!("jade-cwd-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("proj").join("src")).unwrap();
        std::fs::write(base.join("proj").join("src").join("main.cpp"), "").unwrap();
        Fixture(base)
    }

    #[test]
    fn selected_directory_wins() {
        let f = fixture("dir");
        let src = f.0.join("proj").join("src");
        let root = f.0.join("proj");
        assert_eq!(terminal_cwd_for(Some(&src), None, &root), src);
    }

    #[test]
    fn selected_file_uses_its_parent() {
        let f = fixture("file");
        let file = f.0.join("proj").join("src").join("main.cpp");
        let root = f.0.join("proj");
        assert_eq!(terminal_cwd_for(Some(&file), None, &root), file.parent().unwrap());
    }

    #[test]
    fn falls_back_to_active_file_then_root() {
        let f = fixture("fallback");
        let file = f.0.join("proj").join("src").join("main.cpp");
        let root = f.0.join("proj");
        assert_eq!(terminal_cwd_for(None, Some(&file), &root), file.parent().unwrap());
        assert_eq!(terminal_cwd_for(None, None, &root), root);
    }

    /// A selection that no longer exists must not become the cwd.
    #[test]
    fn deleted_selection_falls_through() {
        let f = fixture("gone");
        let root = f.0.join("proj");
        let gone = f.0.join("proj").join("deleted-dir");
        assert_eq!(terminal_cwd_for(Some(&gone), None, &root), root);
        assert_eq!(terminal_cwd_for(Some(Path::new("/nope/x.cpp")), None, &root), root);
    }
}
