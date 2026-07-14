//! Debug session model + breakpoints (feature inventory §5.8 / §4.6).
//!
//! Pure state the LLDB driver feeds and `panels::debug_panel` renders:
//!   - [`DebugStatus`] — running (muted) / paused (amber) / exited (italic).
//!   - [`DebugSession`] — the FRAMES list, the expandable VARIABLES tree (with an
//!     expansion **path set** that survives steps, `panels/debug-panel.ts`), and
//!     the ANSI-stripped CONSOLE column.
//!   - [`Breakpoints`] — per-file line sets (persisted in the `ui` blob, live-
//!     synced to the driver). [`Breakpoints::toggle`] reports whether the click
//!     added or removed a line so the caller can `set_breakpoint`/`remove_breakpoint`.
//!
//! The driver crate (`forge-debug`) owns the parsing; this module owns only the
//! UI-facing bookkeeping and is unit-tested without a live lldb.

use std::collections::{HashMap, HashSet};

use forge_debug::{Breakpoint, LocalVariable, StackFrame};

/// Debug status for the header status line (§5.8: running muted / paused amber
/// bold / exited italic).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DebugStatus {
    /// No session (panel hidden).
    #[default]
    Idle,
    /// Process is running (between stops).
    Running,
    /// Process stopped at a breakpoint / after a step.
    Paused,
    /// Process exited.
    Exited,
}

/// The live debug session state (§5.8). Frames + locals are replaced on each
/// `Stopped`; `expanded` (a set of lldb expression paths) persists across steps
/// so the variables tree keeps its shape; `children` caches lazily-fetched
/// sub-nodes keyed by the same path.
#[derive(Default)]
pub struct DebugSession {
    pub status: DebugStatus,
    pub exit_code: Option<i32>,
    /// Last stop reason (e.g. `breakpoint 1.1`, `step over`).
    pub reason: String,
    /// Current stop location `(file, 1-based line)`.
    pub location: Option<(String, u32)>,
    /// Backtrace frames (frame 0 is the innermost / active by default).
    pub frames: Vec<StackFrame>,
    /// Which frame is selected (click navigates; §5.8 "frame 0 active").
    pub active_frame: usize,
    /// Top-level locals of the (active) frame.
    pub locals: Vec<LocalVariable>,
    /// Expanded variable paths — survives steps (§5.8 "expansion state survives
    /// steps via path set").
    pub expanded: HashSet<String>,
    /// Lazily-fetched children by lldb path (`get_var_children`).
    pub children: HashMap<String, Vec<LocalVariable>>,
    /// ANSI-stripped console scrollback (§5.8 CONSOLE).
    pub console: Vec<String>,
}

/// Console scrollback cap (keeps the paused-at-a-loop firehose bounded).
const CONSOLE_MAX: usize = 5000;

impl DebugSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the session as running (build finished, process launched). Keeps the
    /// console + expansion set; clears the transient stop snapshot.
    pub fn on_running(&mut self) {
        self.status = DebugStatus::Running;
        self.exit_code = None;
        self.frames.clear();
        self.locals.clear();
        self.location = None;
    }

    /// Apply a `Stopped` event: replace frames/locals, reset to frame 0, keep the
    /// expansion set, and invalidate the child cache (values changed — expanded
    /// nodes are re-fetched by the caller). Returns the paths that were expanded
    /// so the caller can re-request their children for the new stop.
    pub fn on_stopped(
        &mut self,
        reason: String,
        file: String,
        line: u32,
        frames: Vec<StackFrame>,
        locals: Vec<LocalVariable>,
    ) -> Vec<String> {
        self.status = DebugStatus::Paused;
        self.reason = reason;
        self.location = Some((file, line));
        self.frames = frames;
        self.active_frame = 0;
        self.locals = locals;
        self.children.clear();
        self.expanded.iter().cloned().collect()
    }

    /// Apply an `Exited` event.
    pub fn on_exited(&mut self, code: i32) {
        self.status = DebugStatus::Exited;
        self.exit_code = Some(code);
        self.frames.clear();
        self.locals.clear();
        self.location = None;
    }

    /// Append a console line (ANSI already stripped by [`push_console`]).
    pub fn push_console(&mut self, text: &str) {
        crate::output::push_output(&mut self.console, text);
        if self.console.len() > CONSOLE_MAX {
            let drop = self.console.len() - CONSOLE_MAX;
            self.console.drain(0..drop);
        }
    }

    /// Whether a variable path is currently expanded.
    pub fn is_expanded(&self, path: &str) -> bool {
        self.expanded.contains(path)
    }

    /// Toggle a variable's expansion. Returns `Some(path)` to fetch children when
    /// it just expanded and no children are cached yet; `None` on collapse or when
    /// children are already present.
    pub fn toggle_var(&mut self, path: &str) -> Option<String> {
        if self.expanded.remove(path) {
            return None; // collapsed
        }
        self.expanded.insert(path.to_string());
        if self.children.contains_key(path) {
            None
        } else {
            Some(path.to_string())
        }
    }

    /// Store lazily-fetched children for a path.
    pub fn set_children(&mut self, path: String, children: Vec<LocalVariable>) {
        self.children.insert(path, children);
    }

    /// Select a stack frame (click-navigate). Clamped to the frame list.
    pub fn select_frame(&mut self, index: usize) {
        if index < self.frames.len() {
            self.active_frame = index;
        }
    }
}

/// A row in the flattened variables tree, ready to render (§5.8 VARIABLES).
pub struct VarRow<'a> {
    pub var: &'a LocalVariable,
    /// Nesting depth (0 = top level) for the indent.
    pub depth: usize,
    /// True when the row can be expanded (`expandable` and has a path).
    pub expandable: bool,
    /// True when the row is currently expanded.
    pub expanded: bool,
}

/// Flatten the variables tree into render rows, honoring the expansion set and
/// the lazily-fetched child cache. A depth-first walk: an expanded node with
/// cached children yields those children (recursively) right after it.
pub fn flatten_vars<'a>(session: &'a DebugSession) -> Vec<VarRow<'a>> {
    let mut out = Vec::new();
    walk_vars(session, &session.locals, 0, &mut out);
    out
}

fn walk_vars<'a>(
    session: &'a DebugSession,
    vars: &'a [LocalVariable],
    depth: usize,
    out: &mut Vec<VarRow<'a>>,
) {
    for v in vars {
        let path = v.path.as_deref();
        let expandable = v.expandable.unwrap_or(false) && path.is_some();
        let expanded = path.map(|p| session.is_expanded(p)).unwrap_or(false);
        out.push(VarRow {
            var: v,
            depth,
            expandable,
            expanded,
        });
        if expanded {
            if let Some(p) = path {
                // Prefer inline children, else the lazily-fetched cache.
                if let Some(kids) = v.children.as_deref() {
                    walk_vars(session, kids, depth + 1, out);
                } else if let Some(kids) = session.children.get(p) {
                    walk_vars(session, kids, depth + 1, out);
                }
            }
        }
    }
}

/// Whether a `Removed` or `Added` breakpoint toggle occurred, so the caller can
/// sync the driver (`set_breakpoint` / `remove_breakpoint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakpointChange {
    Added,
    Removed,
}

/// Per-file breakpoint line sets (§4.6). Serialized shape is the Electron
/// `Record<string, number[]>` (`state.ts` key `breakpoints`), so it round-trips
/// through the `ui` blob unchanged. Lines are kept sorted + de-duped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Breakpoints {
    map: HashMap<String, Vec<u32>>,
}

impl Breakpoints {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a persisted map (normalizes each line vec: sorted + de-duped).
    pub fn from_map(map: HashMap<String, Vec<u32>>) -> Self {
        let mut bp = Breakpoints { map };
        for lines in bp.map.values_mut() {
            lines.sort_unstable();
            lines.dedup();
        }
        bp.map.retain(|_, lines| !lines.is_empty());
        bp
    }

    /// The persisted map (for the `ui` blob).
    pub fn to_map(&self) -> HashMap<String, Vec<u32>> {
        self.map.clone()
    }

    /// Lines set on `file` (empty when none).
    pub fn lines_for(&self, file: &str) -> &[u32] {
        self.map.get(file).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether `line` is a breakpoint in `file`.
    pub fn is_set(&self, file: &str, line: u32) -> bool {
        self.map.get(file).is_some_and(|l| l.binary_search(&line).is_ok())
    }

    /// Toggle a breakpoint, returning whether it was added or removed. Keeps the
    /// per-file vec sorted so `is_set` can binary-search.
    pub fn toggle(&mut self, file: &str, line: u32) -> BreakpointChange {
        let lines = self.map.entry(file.to_string()).or_default();
        match lines.binary_search(&line) {
            Ok(pos) => {
                lines.remove(pos);
                if lines.is_empty() {
                    self.map.remove(file);
                }
                BreakpointChange::Removed
            }
            Err(pos) => {
                lines.insert(pos, line);
                BreakpointChange::Added
            }
        }
    }

    /// All breakpoints as driver [`Breakpoint`]s (for `start`), file-order stable.
    pub fn all(&self) -> Vec<Breakpoint> {
        let mut out = Vec::new();
        let mut files: Vec<&String> = self.map.keys().collect();
        files.sort();
        for f in files {
            for &line in &self.map[f] {
                out.push(Breakpoint::new(f.clone(), line));
            }
        }
        out
    }

    /// Total breakpoint count (smoke / test metric).
    pub fn count(&self) -> usize {
        self.map.values().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str, path: Option<&str>, expandable: bool) -> LocalVariable {
        LocalVariable {
            name: name.to_string(),
            type_: "int".to_string(),
            value: "0".to_string(),
            path: path.map(str::to_string),
            children: None,
            expandable: Some(expandable),
        }
    }

    fn frame(index: u32, name: &str, line: u32) -> StackFrame {
        StackFrame {
            index,
            function_name: name.to_string(),
            file: "main.cpp".to_string(),
            line,
        }
    }

    #[test]
    fn breakpoint_toggle_add_remove_and_sync_signal() {
        let mut bp = Breakpoints::new();
        assert_eq!(bp.toggle("a.cpp", 10), BreakpointChange::Added);
        assert_eq!(bp.toggle("a.cpp", 5), BreakpointChange::Added);
        assert!(bp.is_set("a.cpp", 10));
        assert!(bp.is_set("a.cpp", 5));
        // Kept sorted so is_set can binary-search.
        assert_eq!(bp.lines_for("a.cpp"), &[5, 10]);
        // Toggling an existing line removes it (and signals Removed for the driver).
        assert_eq!(bp.toggle("a.cpp", 10), BreakpointChange::Removed);
        assert!(!bp.is_set("a.cpp", 10));
        assert_eq!(bp.lines_for("a.cpp"), &[5]);
        // Removing the last line drops the file entry.
        assert_eq!(bp.toggle("a.cpp", 5), BreakpointChange::Removed);
        assert!(bp.lines_for("a.cpp").is_empty());
        assert_eq!(bp.count(), 0);
    }

    #[test]
    fn breakpoints_roundtrip_map_normalizes() {
        let mut map = HashMap::new();
        map.insert("x.cpp".to_string(), vec![30u32, 10, 10, 20]); // unsorted + dupe
        map.insert("empty.cpp".to_string(), vec![]);
        let bp = Breakpoints::from_map(map);
        assert_eq!(bp.lines_for("x.cpp"), &[10, 20, 30]);
        assert!(bp.lines_for("empty.cpp").is_empty()); // empty file pruned
        // all() is stable + covers every line as a driver Breakpoint.
        let all = bp.all();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], Breakpoint::new("x.cpp", 10));
    }

    #[test]
    fn session_stop_keeps_expansion_and_requests_children() {
        let mut s = DebugSession::new();
        s.expanded.insert("m".to_string());
        s.expanded.insert("v".to_string());
        let to_fetch = s.on_stopped(
            "breakpoint 1.1".to_string(),
            "main.cpp".to_string(),
            42,
            vec![frame(0, "main", 42), frame(1, "run", 12)],
            vec![var("m", Some("m"), true)],
        );
        assert_eq!(s.status, DebugStatus::Paused);
        assert_eq!(s.active_frame, 0);
        assert_eq!(s.frames.len(), 2);
        // Both previously-expanded paths are returned for a re-fetch, and the
        // child cache was invalidated by the new stop.
        assert!(s.children.is_empty());
        let mut sorted = to_fetch.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["m".to_string(), "v".to_string()]);
    }

    #[test]
    fn var_toggle_and_flatten_tree() {
        let mut s = DebugSession::new();
        s.locals = vec![
            var("m", Some("m"), true),
            var("n", None, false), // non-expandable scalar
        ];
        // Before expansion: two top-level rows, `m` expandable but collapsed.
        let rows = flatten_vars(&s);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].expandable && !rows[0].expanded);
        assert!(!rows[1].expandable);

        // Toggle `m` → expands, and asks for a child fetch (nothing cached).
        assert_eq!(s.toggle_var("m").as_deref(), Some("m"));
        // Supply children, then flatten shows the nested row.
        s.set_children("m".to_string(), vec![var("m.q", Some("m.q"), false)]);
        let rows = flatten_vars(&s);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].var.name, "m.q");

        // Toggling again collapses (no fetch) and hides the child.
        assert_eq!(s.toggle_var("m"), None);
        assert_eq!(flatten_vars(&s).len(), 2);
    }

    #[test]
    fn console_strips_ansi() {
        let mut s = DebugSession::new();
        s.push_console("\x1b[32mrunning\x1b[0m\n");
        assert_eq!(s.console, vec!["running".to_string()]);
    }
}
