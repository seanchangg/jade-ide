//! Tabs + read-only viewer state (deliverable §3).
//!
//! Open dedupes by path (like `editor-manager.ts`); each tab reads its file once
//! (utf-8 lossy), highlights it once (read-only — editing is a later step), and
//! stores per-line text + spans. Closing a tab adjusts the active index exactly
//! as `editor-manager.ts:208-241`. The active tab drives `JadeApp::active_file`
//! so Build/Run target the front tab.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::decorations::flow::{self, FlowAnalysis};
use crate::decorations::size_annotations;
use crate::highlight::{self, Span, TokenPalette};

/// One open, highlighted file. Read-only for now: `lines` and `highlights` are
/// parallel (index `i` = line `i`), computed once at open time.
#[derive(Debug, Clone)]
pub struct OpenTab {
    pub path: PathBuf,
    pub name: String,
    pub lines: Vec<String>,
    /// Per-line colored spans (byte ranges relative to the line start). Empty for
    /// non-highlightable files (plain text).
    pub highlights: Vec<Vec<Span>>,
    /// Static size annotations by 1-based line (§4.5 system 1). Recomputed once
    /// per open/switch — read-only, so no content-change debounce is needed (the
    /// TS debounces at `:130-134` only matter once editing lands in step 3b).
    pub sizes: HashMap<usize, String>,
    /// Execution-flow analysis (§4.8), likewise cached at open time.
    pub flow: FlowAnalysis,
}

impl OpenTab {
    /// Read + highlight a file. utf-8 lossy so any file is viewable.
    pub fn from_file(path: &Path, palette: TokenPalette) -> std::io::Result<OpenTab> {
        let raw = std::fs::read(path)?;
        let text = String::from_utf8_lossy(&raw).into_owned();
        Ok(Self::from_text(path, &text, palette))
    }

    /// Build a tab from already-loaded text (used by the fs path and by tests).
    /// Lines are split on `\n`; `highlight_file` produces a matching per-line
    /// span vector (same split), so the two stay index-aligned.
    pub fn from_text(path: &Path, text: &str, palette: TokenPalette) -> OpenTab {
        let highlights = highlight::highlight_file(path, text, palette);
        let lines: Vec<String> = text.split('\n').map(|l| l.to_string()).collect();
        debug_assert_eq!(lines.len(), highlights.len());
        // Decoration analysis is only meaningful for C-family/Metal sources; for
        // plain text both scanners simply yield nothing.
        let (sizes, flow) = if highlight::is_highlightable(path) {
            (
                size_annotations::collect_size_annotations(text),
                flow::analyze(text),
            )
        } else {
            (HashMap::new(), FlowAnalysis::default())
        };
        OpenTab {
            path: path.to_path_buf(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            lines,
            highlights,
            sizes,
            flow,
        }
    }

    /// Total highlighted spans across all lines (headless smoke metric).
    pub fn span_count(&self) -> usize {
        self.highlights.iter().map(|l| l.len()).sum()
    }
}

/// The tab strip + viewer state.
#[derive(Debug, Default)]
pub struct EditorState {
    pub tabs: Vec<OpenTab>,
    /// Index of the active tab, or `None` when no tabs are open.
    pub active: Option<usize>,
    palette: TokenPalette,
}

impl EditorState {
    pub fn new(palette: TokenPalette) -> Self {
        EditorState {
            tabs: Vec::new(),
            active: None,
            palette,
        }
    }

    /// Index of an already-open tab for `path`, if any.
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.tabs.iter().position(|t| t.path == path)
    }

    /// Open a file: dedupe by path (just switch if already open), else read +
    /// highlight it once and append. The opened tab becomes active. Returns the
    /// active index, or an IO error if the file could not be read.
    pub fn open(&mut self, path: &Path) -> std::io::Result<usize> {
        if let Some(i) = self.index_of(path) {
            self.active = Some(i);
            return Ok(i);
        }
        let tab = OpenTab::from_file(path, self.palette)?;
        Ok(self.push_tab(tab))
    }

    /// Append (or dedupe) an already-built tab; the tab becomes active. Returns
    /// the active index. Split out so tests exercise dedupe/close without fs.
    pub fn push_tab(&mut self, tab: OpenTab) -> usize {
        if let Some(i) = self.index_of(&tab.path) {
            self.active = Some(i);
            return i;
        }
        self.tabs.push(tab);
        let i = self.tabs.len() - 1;
        self.active = Some(i);
        i
    }

    /// Switch the active tab (no-op for an out-of-range index).
    pub fn switch(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = Some(index);
        }
    }

    /// Close a tab, adjusting the active index exactly like
    /// `editor-manager.ts:208-241`:
    ///   - no tabs left → active = None;
    ///   - closed the active tab → activate `min(index, len-1)`;
    ///   - closed a tab before the active one → shift active left by one;
    ///   - otherwise the active tab is unchanged.
    pub fn close(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let was_active = self.active == Some(index);
        self.tabs.remove(index);

        if self.tabs.is_empty() {
            self.active = None;
        } else if was_active {
            self.active = Some(index.min(self.tabs.len() - 1));
        } else if let Some(a) = self.active {
            if index < a {
                self.active = Some(a - 1);
            }
        }
    }

    /// The active tab, if any.
    pub fn active_tab(&self) -> Option<&OpenTab> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    /// The active tab's path — used to keep `JadeApp::active_file` in sync so the
    /// Build/Run target follows the front tab.
    pub fn active_path(&self) -> Option<PathBuf> {
        self.active_tab().map(|t| t.path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(name: &str) -> OpenTab {
        OpenTab {
            path: PathBuf::from(format!("/x/{name}")),
            name: name.to_string(),
            lines: vec![String::new()],
            highlights: vec![Vec::new()],
            sizes: HashMap::new(),
            flow: FlowAnalysis::default(),
        }
    }

    #[test]
    fn open_dedupes_by_path() {
        let mut e = EditorState::new(TokenPalette::forge_dark());
        e.push_tab(tab("a.cpp"));
        e.push_tab(tab("b.cpp"));
        assert_eq!(e.tabs.len(), 2);
        assert_eq!(e.active, Some(1));

        // Re-opening a.cpp switches to it instead of adding a duplicate.
        e.push_tab(tab("a.cpp"));
        assert_eq!(e.tabs.len(), 2);
        assert_eq!(e.active, Some(0));
    }

    #[test]
    fn close_active_activates_nearest() {
        let mut e = EditorState::new(TokenPalette::forge_dark());
        e.push_tab(tab("a"));
        e.push_tab(tab("b"));
        e.push_tab(tab("c"));
        e.switch(1); // active = b
        e.close(1); // close b (active) → min(1, len-1=1) = index 1 (was c)
        assert_eq!(e.tabs.len(), 2);
        assert_eq!(e.active, Some(1));
        assert_eq!(e.active_tab().unwrap().name, "c");
    }

    #[test]
    fn close_last_active_clamps() {
        let mut e = EditorState::new(TokenPalette::forge_dark());
        e.push_tab(tab("a"));
        e.push_tab(tab("b"));
        e.switch(1); // active = b (last)
        e.close(1); // → min(1, 0) = 0
        assert_eq!(e.active, Some(0));
        assert_eq!(e.active_tab().unwrap().name, "a");
    }

    #[test]
    fn close_before_active_shifts_left() {
        let mut e = EditorState::new(TokenPalette::forge_dark());
        e.push_tab(tab("a"));
        e.push_tab(tab("b"));
        e.push_tab(tab("c"));
        e.switch(2); // active = c
        e.close(0); // close a (before active) → active shifts 2 → 1
        assert_eq!(e.active, Some(1));
        assert_eq!(e.active_tab().unwrap().name, "c");
    }

    #[test]
    fn close_after_active_keeps_active() {
        let mut e = EditorState::new(TokenPalette::forge_dark());
        e.push_tab(tab("a"));
        e.push_tab(tab("b"));
        e.push_tab(tab("c"));
        e.switch(0); // active = a
        e.close(2); // close c (after active) → active unchanged
        assert_eq!(e.active, Some(0));
    }

    #[test]
    fn close_all_clears_active() {
        let mut e = EditorState::new(TokenPalette::forge_dark());
        e.push_tab(tab("a"));
        e.close(0);
        assert_eq!(e.active, None);
        assert!(e.active_path().is_none());
    }

    #[test]
    fn from_text_lines_align_with_highlights() {
        let text = "int x = 1;\n// c\n";
        let t = OpenTab::from_text(&PathBuf::from("a.cpp"), text, TokenPalette::forge_dark());
        assert_eq!(t.lines.len(), t.highlights.len());
        assert!(t.span_count() > 0);
    }
}
