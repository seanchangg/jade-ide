//! Tabs + editable buffer-backed editor state (Phase-3b E2).
//!
//! Each [`OpenTab`] now owns a [`forge_buffer::Buffer`] (the canonical text) plus
//! cached per-line highlight spans and decoration analyses that are recomputed as
//! the text changes. `EditorState` keeps the tab strip semantics (dedupe by path,
//! close-index math) from the read-only viewer; the active tab drives
//! `JadeApp::active_file`.
//!
//! ## Highlight invalidation choice
//! On every text change we re-highlight the **whole file** (`rehighlight`). At the
//! single-source-file scale Jade targets (a few thousand lines) a full tree-sitter
//! reparse is sub-millisecond and keeps the code path trivially correct — there is
//! no incremental-edit bookkeeping to get wrong. Tree-sitter's incremental `edit`
//! API would let us reparse only the touched range, but the win is negligible here
//! and the complexity (byte/point deltas fed to `Tree::edit`) is not worth it yet;
//! this is the documented, deliberate choice.
//!
//! ## Decoration debounces (inventory §10)
//! Highlighting + the cheap static size scan run immediately on edit (needed for a
//! correct next frame). The heavier flow analysis (300ms) and tree-sitter symbol
//! outline (800ms) run on per-tab [`Debounce`]s, driven by a wake task in `app.rs`
//! and unit-tested here in isolation.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use forge_buffer::{Buffer, EditRecord, LspPosition, Point, Selection};
use forge_lsp::{CompletionItem, Diagnostic, DiagnosticSeverity, Position as LspRange};
use lsp_types::CompletionTextEdit;

use crate::decorations::flow::{self, FlowAnalysis};
use crate::decorations::size_annotations;
use crate::highlight::{self, Span, TokenPalette};
use crate::structure::{self, Symbol};

/// Static size-annotation rescan delay (inventory §10: memory rescan 1000ms;
/// the 50ms coalesce lives in the app's wake-loop poll granularity).
pub const SIZES_DEBOUNCE_MS: u64 = 1000;
/// Flow-decoration recompute delay (inventory §10: 300ms).
pub const FLOW_DEBOUNCE_MS: u64 = 300;
/// Structure-outline recompute delay (inventory §10: 800ms).
pub const STRUCT_DEBOUNCE_MS: u64 = 800;

/// One open, editable file.
///
/// `buffer` is the source of truth; `highlights`/`sizes` track it eagerly,
/// `flow`/`symbols` track it on the debounces. `diagnostics` are the last
/// clangd `publishDiagnostics` for this path.
pub struct OpenTab {
    pub path: PathBuf,
    pub name: String,
    /// The editable text buffer (cursors, undo, dirty tracking live here).
    pub buffer: Buffer,
    /// Per-line colored spans (byte ranges relative to the line start). Empty for
    /// non-highlightable files. Recomputed on every edit (`rehighlight`).
    pub highlights: Vec<Vec<Span>>,
    /// Static size annotations by 1-based line (§4.5 system 1).
    pub sizes: HashMap<usize, String>,
    /// Execution-flow analysis (§4.8), recomputed on [`Self::flow_debounce`].
    pub flow: FlowAnalysis,
    /// Tree-sitter symbol outline (§5.5), recomputed on [`Self::struct_debounce`].
    pub symbols: Vec<Symbol>,
    /// Last diagnostics clangd published for this file.
    pub diagnostics: Vec<Diagnostic>,
    /// IME composition range (byte range in the buffer), rendered with an
    /// underline. `None` when not composing.
    pub marked: Option<Range<usize>>,
    /// True once `didOpen` was sent for this tab (so we don't double-open).
    pub lsp_opened: bool,
    /// Debounce for the static size-annotation rescan (§10: 1000ms).
    pub sizes_debounce: Debounce,
    /// Debounce for the flow recompute (§10: 300ms).
    pub flow_debounce: Debounce,
    /// Debounce for the symbol-outline recompute (§10: 800ms).
    pub struct_debounce: Debounce,
    palette: TokenPalette,
}

impl OpenTab {
    /// Read + build an editable tab from a file (utf-8 lossy so any file opens).
    pub fn from_file(path: &Path, palette: TokenPalette) -> std::io::Result<OpenTab> {
        let raw = std::fs::read(path)?;
        let text = String::from_utf8_lossy(&raw).into_owned();
        Ok(Self::from_text(path, &text, palette))
    }

    /// Build a tab from already-loaded text (fs path + tests + smokes).
    pub fn from_text(path: &Path, text: &str, palette: TokenPalette) -> OpenTab {
        let buffer = Buffer::from_text(text);
        let highlightable = highlight::is_highlightable(path);
        let highlights = highlight::highlight_file(path, text, palette);
        let (sizes, flow, symbols) = if highlightable {
            (
                size_annotations::collect_size_annotations(text),
                flow::analyze(text),
                structure::parse_symbols(text),
            )
        } else {
            (HashMap::new(), FlowAnalysis::default(), Vec::new())
        };
        OpenTab {
            path: path.to_path_buf(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            buffer,
            highlights,
            sizes,
            flow,
            symbols,
            diagnostics: Vec::new(),
            marked: None,
            lsp_opened: false,
            sizes_debounce: Debounce::new(SIZES_DEBOUNCE_MS),
            flow_debounce: Debounce::new(FLOW_DEBOUNCE_MS),
            struct_debounce: Debounce::new(STRUCT_DEBOUNCE_MS),
            palette,
        }
    }

    /// The text of line `row` (trailing `\n` stripped), as an owned String.
    pub fn line(&self, row: usize) -> String {
        self.buffer.line(row).into_owned()
    }

    /// Number of lines (matches `text.split('\n').count()`).
    pub fn line_count(&self) -> usize {
        self.buffer.line_count()
    }

    /// Total highlighted spans across all lines (headless smoke metric).
    pub fn span_count(&self) -> usize {
        self.highlights.iter().map(|l| l.len()).sum()
    }

    /// Whether this file's extension is highlighted / decorated.
    pub fn is_code(&self) -> bool {
        highlight::is_highlightable(&self.path)
    }

    /// Post-edit bookkeeping: re-highlight eagerly (rendering must be correct on
    /// the very next frame) and arm the three decoration debounces (§10: sizes
    /// 1000ms, flow 300ms, structure 800ms). Called from every edit entry point.
    pub fn on_edited(&mut self, now_ms: u64) {
        self.rehighlight();
        self.sizes_debounce.arm(now_ms);
        self.flow_debounce.arm(now_ms);
        self.struct_debounce.arm(now_ms);
    }

    /// Full-file re-highlight (see the module-level invalidation note).
    pub fn rehighlight(&mut self) {
        let text = self.buffer.to_string();
        self.highlights = highlight::highlight_file(&self.path, &text, self.palette);
    }

    /// Swap the token palette (theme toggle, §4.2) and re-highlight this tab.
    pub fn set_palette(&mut self, palette: TokenPalette) {
        self.palette = palette;
        self.rehighlight();
    }

    /// Run any decoration recompute whose debounce has elapsed. Returns true if
    /// anything was recomputed (so the caller can `notify`).
    pub fn poll_decorations(&mut self, now_ms: u64) -> bool {
        let mut changed = false;
        if self.sizes_debounce.poll(now_ms) {
            if self.is_code() {
                self.sizes = size_annotations::collect_size_annotations(&self.buffer.to_string());
            }
            changed = true;
        }
        if self.flow_debounce.poll(now_ms) {
            if self.is_code() {
                self.flow = flow::analyze(&self.buffer.to_string());
            }
            changed = true;
        }
        if self.struct_debounce.poll(now_ms) {
            if self.is_code() {
                self.symbols = structure::parse_symbols(&self.buffer.to_string());
            }
            changed = true;
        }
        changed
    }

    /// True while a decoration recompute is still pending.
    pub fn decorations_pending(&self) -> bool {
        self.sizes_debounce.is_pending()
            || self.flow_debounce.is_pending()
            || self.struct_debounce.is_pending()
    }

    /// The monotonic LSP document version (buffer version + 1, so `didOpen` is 1).
    pub fn lsp_version(&self) -> i32 {
        (self.buffer.version() + 1) as i32
    }

    /// The primary caret as a render Point (row + char column).
    pub fn caret_point(&self) -> Point {
        self.buffer.offset_to_point(self.buffer.selection().caret())
    }
}

/// A pure millisecond-window debounce (mirrors `workspace_tree::WatchDebounce`,
/// kept editor-local). `arm` (re)starts the window; `poll` fires once the delay
/// has elapsed since the last `arm`. Deterministic: the caller supplies the clock.
#[derive(Debug, Clone)]
pub struct Debounce {
    delay_ms: u64,
    armed_at: Option<u64>,
}

impl Debounce {
    pub fn new(delay_ms: u64) -> Self {
        Debounce {
            delay_ms,
            armed_at: None,
        }
    }

    /// (Re)arm the window at `now_ms`.
    pub fn arm(&mut self, now_ms: u64) {
        self.armed_at = Some(now_ms);
    }

    /// Fire (consuming the armed state) if the delay has elapsed since `arm`.
    pub fn poll(&mut self, now_ms: u64) -> bool {
        if let Some(t) = self.armed_at {
            if now_ms.saturating_sub(t) >= self.delay_ms {
                self.armed_at = None;
                return true;
            }
        }
        false
    }

    pub fn is_pending(&self) -> bool {
        self.armed_at.is_some()
    }
}

/// The tab strip + editor state.
#[derive(Default)]
pub struct EditorState {
    pub tabs: Vec<OpenTab>,
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

    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.tabs.iter().position(|t| t.path == path)
    }

    /// Open a file: dedupe by path, else read + build an editable tab and append.
    pub fn open(&mut self, path: &Path) -> std::io::Result<usize> {
        if let Some(i) = self.index_of(path) {
            self.active = Some(i);
            return Ok(i);
        }
        let tab = OpenTab::from_file(path, self.palette)?;
        Ok(self.push_tab(tab))
    }

    /// Append (or dedupe) an already-built tab; it becomes active.
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

    pub fn switch(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = Some(index);
        }
    }

    /// Swap the token palette for new tabs and re-highlight every open tab
    /// (theme toggle, §4.2).
    pub fn set_palette(&mut self, palette: TokenPalette) {
        self.palette = palette;
        for tab in &mut self.tabs {
            tab.set_palette(palette);
        }
    }

    /// Close a tab, adjusting the active index (editor-manager.ts:208-241).
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

    pub fn active_tab(&self) -> Option<&OpenTab> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut OpenTab> {
        match self.active {
            Some(i) => self.tabs.get_mut(i),
            None => None,
        }
    }

    pub fn active_path(&self) -> Option<PathBuf> {
        self.active_tab().map(|t| t.path.clone())
    }

    /// Find the tab for `path` (diagnostics routing).
    pub fn tab_mut_for(&mut self, path: &Path) -> Option<&mut OpenTab> {
        self.tabs.iter_mut().find(|t| t.path == path)
    }
}

// ── Pure editor logic (unit-tested below) ─────────────────────────────────────

/// A per-byte combined style cell used to flatten overlapping style layers
/// (syntax color · selection · diagnostic underline · IME marked) into the
/// sorted, non-overlapping run list `gpui::StyledText::with_highlights` requires.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    /// Syntax color (`0xRRGGBB`), if any.
    pub color: Option<u32>,
    /// True when this byte is inside the selection (rendered as a bg wash).
    pub selected: bool,
    /// Diagnostic underline color (`0xRRGGBB`), if any.
    pub underline: Option<u32>,
    /// True when this byte is inside the IME marked (composing) range.
    pub marked: bool,
}

/// Flatten the style layers for one line into coalesced `(byte-range, CellStyle)`
/// runs. Inputs are all in **line-relative byte** coordinates and are assumed to
/// fall on char boundaries (they come from tree-sitter nodes / char-aligned
/// offsets), so runs never split a multi-byte char.
pub fn merge_line_styles(
    len: usize,
    syntax: &[Span],
    selection: Option<Range<usize>>,
    underlines: &[(Range<usize>, u32)],
    marked: Option<Range<usize>>,
) -> Vec<(Range<usize>, CellStyle)> {
    if len == 0 {
        return Vec::new();
    }
    let mut cells = vec![CellStyle::default(); len];
    for s in syntax {
        for cell in &mut cells[s.start.min(len)..s.end.min(len)] {
            cell.color = Some(s.color);
        }
    }
    if let Some(sel) = selection {
        for cell in &mut cells[sel.start.min(len)..sel.end.min(len)] {
            cell.selected = true;
        }
    }
    for (r, color) in underlines {
        for cell in &mut cells[r.start.min(len)..r.end.min(len)] {
            cell.underline = Some(*color);
        }
    }
    if let Some(m) = marked {
        for cell in &mut cells[m.start.min(len)..m.end.min(len)] {
            cell.marked = true;
        }
    }
    // Coalesce equal adjacent cells into runs.
    let mut runs = Vec::new();
    let mut start = 0;
    for i in 1..=len {
        if i == len || cells[i] != cells[start] {
            let style = cells[start];
            if style != CellStyle::default() {
                runs.push((start..i, style));
            }
            start = i;
        }
    }
    runs
}

/// Round a line-relative x pixel to a char column, clamped to `max_col`.
pub fn px_to_col(x_rel: f32, char_w: f32, max_col: usize) -> usize {
    if x_rel <= 0.0 || char_w <= 0.0 {
        return 0;
    }
    ((x_rel / char_w).round() as usize).min(max_col)
}

/// The x pixel of char column `col`.
pub fn col_to_px(col: usize, char_w: f32) -> f32 {
    col as f32 * char_w
}

/// The char-column range of the word/identifier at char column `col` on `line`
/// (double-click selection). A word is `[A-Za-z0-9_]`; clicking on a non-word
/// char selects just that char.
pub fn word_range_in_line(line: &str, col: usize) -> Range<usize> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    if n == 0 {
        return 0..0;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    // Clicking at end-of-line or on a boundary: look at the char under the caret,
    // falling back to the char to the left.
    let idx = col.min(n.saturating_sub(1));
    if !is_word(chars[idx]) {
        // Non-word: select the single char (or nothing at line end).
        return if col >= n { n..n } else { col..col + 1 };
    }
    let mut start = idx;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = idx + 1;
    while end < n && is_word(chars[end]) {
        end += 1;
    }
    start..end
}

/// The prefix identifier immediately left of `caret` on its line: returns the
/// buffer byte range of that identifier (empty range at the caret when there is
/// no identifier char to the left). Used to know what a completion replaces and
/// to filter the list by what's typed.
pub fn ident_range_before(buffer: &Buffer, caret: usize) -> Range<usize> {
    let point = buffer.offset_to_point(caret);
    let line = buffer.line(point.row);
    let chars: Vec<char> = line.chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut start_col = point.col;
    while start_col > 0 && start_col <= chars.len() && is_word(chars[start_col - 1]) {
        start_col -= 1;
    }
    let start = buffer.point_to_offset(Point::new(point.row, start_col));
    start..caret
}

/// The severity → underline color mapping (§4.2): error red, warning amber, info
/// blue, hint muted-blue.
pub fn severity_color(sev: Option<DiagnosticSeverity>, red: u32, amber: u32, blue: u32) -> u32 {
    match sev {
        Some(DiagnosticSeverity::ERROR) => red,
        Some(DiagnosticSeverity::WARNING) => amber,
        _ => blue,
    }
}

/// The utf16-column span a diagnostic covers **on a single row**, or `None` when
/// the row is outside the diagnostic's line range. For a multi-line diagnostic
/// the first row runs `start.col..line_end`, interior rows the whole line
/// (`0..line_end`), and the last row `0..end.col`. `line_len_u16` is the row's
/// length in utf16 code units.
pub fn diag_span_on_row(
    start: (usize, usize),
    end: (usize, usize),
    row: usize,
    line_len_u16: usize,
) -> Option<(usize, usize)> {
    let (sl, sc) = start;
    let (el, ec) = end;
    if row < sl || row > el {
        return None;
    }
    let s = if row == sl { sc } else { 0 };
    let e = if row == el { ec } else { line_len_u16 };
    let e = e.min(line_len_u16).max(s);
    // A zero-width diagnostic (start == end on the same row) still underlines one
    // column so it's visible.
    if s == e {
        Some((s, (s + 1).min(line_len_u16.max(s + 1))))
    } else {
        Some((s, e))
    }
}

/// Compute the diagnostic underline runs for a row, in **line-relative byte**
/// coordinates, colored by severity. Uses the buffer for utf16→byte conversion.
pub fn diagnostic_underlines_for_row(
    diagnostics: &[Diagnostic],
    row: usize,
    buffer: &Buffer,
    red: u32,
    amber: u32,
    blue: u32,
) -> Vec<(Range<usize>, u32)> {
    let line_start = buffer.point_to_offset(Point::new(row, 0));
    let line_len_u16 = line_utf16_len(buffer, row);
    let mut out = Vec::new();
    for d in diagnostics {
        let start = (d.range.start.line as usize, d.range.start.character as usize);
        let end = (d.range.end.line as usize, d.range.end.character as usize);
        if let Some((sc, ec)) = diag_span_on_row(start, end, row, line_len_u16) {
            let sb = buffer.lsp_to_offset(LspPosition::new(row, sc)) - line_start;
            let eb = buffer.lsp_to_offset(LspPosition::new(row, ec)) - line_start;
            if eb > sb {
                out.push((sb..eb, severity_color(d.severity, red, amber, blue)));
            }
        }
    }
    out
}

/// Count severities across a diagnostics list → (errors, warnings, infos). Hints
/// fold into infos.
pub fn diagnostic_counts(diagnostics: &[Diagnostic]) -> (usize, usize, usize) {
    let mut e = 0;
    let mut w = 0;
    let mut i = 0;
    for d in diagnostics {
        match d.severity {
            Some(DiagnosticSeverity::ERROR) => e += 1,
            Some(DiagnosticSeverity::WARNING) => w += 1,
            _ => i += 1,
        }
    }
    (e, w, i)
}

/// Filter completion items by the typed `prefix` (case-insensitive prefix match
/// against the item's filter text / label), preserving server order. An empty
/// prefix keeps everything.
pub fn completion_filter(items: &[CompletionItem], prefix: &str) -> Vec<usize> {
    if prefix.is_empty() {
        return (0..items.len()).collect();
    }
    let needle = prefix.to_ascii_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            let hay = it
                .filter_text
                .as_deref()
                .unwrap_or(&it.label)
                .to_ascii_lowercase();
            hay.starts_with(&needle)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Resolve a completion item to the concrete buffer edit to apply: `(byte range
/// to replace, replacement text)`. Prefers the item's `textEdit` (whose range is
/// authoritative, converted from LSP utf16), else replaces the typed identifier
/// (`ident_range`) with `insertText`/`label`.
pub fn completion_edit(
    item: &CompletionItem,
    buffer: &Buffer,
    ident_range: Range<usize>,
) -> (Range<usize>, String) {
    if let Some(te) = &item.text_edit {
        let (range, text) = match te {
            CompletionTextEdit::Edit(e) => (&e.range, e.new_text.clone()),
            CompletionTextEdit::InsertAndReplace(e) => (&e.replace, e.new_text.clone()),
        };
        let s = buffer.lsp_to_offset(lsp_pos(range.start));
        let end = buffer.lsp_to_offset(lsp_pos(range.end));
        return (s..end, text);
    }
    let text = item
        .insert_text
        .clone()
        .unwrap_or_else(|| item.label.clone());
    (ident_range, text)
}

fn lsp_pos(p: LspRange) -> LspPosition {
    LspPosition::new(p.line as usize, p.character as usize)
}

/// utf16 length of line `row` (excluding the trailing newline).
pub fn line_utf16_len(buffer: &Buffer, row: usize) -> usize {
    buffer.line(row).chars().map(|c| c.len_utf16()).sum()
}

/// Whole-document byte offset → document utf16 offset (EntityInputHandler speaks
/// document-wide utf16). O(rows) using only the buffer's public API.
pub fn byte_to_doc_utf16(buffer: &Buffer, byte: usize) -> usize {
    let p = buffer.offset_to_point(byte);
    let mut u = 0;
    for r in 0..p.row {
        u += line_utf16_len(buffer, r) + 1; // + newline
    }
    let line = buffer.line(p.row);
    u += line
        .chars()
        .take(p.col)
        .map(|c| c.len_utf16())
        .sum::<usize>();
    u
}

/// Document utf16 offset → whole-document byte offset (inverse of the above).
pub fn doc_utf16_to_byte(buffer: &Buffer, target: usize) -> usize {
    let mut u = 0;
    let lines = buffer.line_count();
    for r in 0..lines {
        let ll = line_utf16_len(buffer, r);
        if target <= u + ll {
            let into = target - u;
            let line = buffer.line(r);
            let mut cu = 0;
            let mut col = 0;
            for c in line.chars() {
                if cu >= into {
                    break;
                }
                cu += c.len_utf16();
                col += 1;
            }
            return buffer.point_to_offset(Point::new(r, col));
        }
        u += ll + 1; // newline
    }
    buffer.len_bytes()
}

/// Convert a buffer [`EditRecord`]'s descending utf16 changes into the LSP wire
/// edits `forge_lsp::DidChange::Incremental` expects.
pub fn record_to_lsp_edits(record: &EditRecord) -> Vec<forge_lsp::Utf16RangeEdit> {
    record
        .changes
        .iter()
        .map(|c| forge_lsp::Utf16RangeEdit {
            range: forge_lsp::Range {
                start: forge_lsp::Position::new(c.start.line as u32, c.start.character as u32),
                end: forge_lsp::Position::new(c.end.line as u32, c.end.character as u32),
            },
            text: c.new_text.clone(),
        })
        .collect()
}

/// Byte range of the primary selection intersected with `[line_start, line_end]`
/// and made line-relative — or `None` when the selection doesn't cover any of
/// this line's text. (An empty selection, or one touching only the line's very
/// end, yields `None` — nothing to wash.)
pub fn selection_on_line(
    sel: Selection,
    line_start: usize,
    line_end: usize,
) -> Option<Range<usize>> {
    let (s, e) = (sel.start(), sel.end());
    if s >= e {
        return None; // empty selection
    }
    if e <= line_start || s >= line_end {
        return None; // no overlap with this line's text
    }
    let cs = s.max(line_start) - line_start;
    let ce = e.min(line_end) - line_start;
    if ce <= cs {
        return None;
    }
    Some(cs..ce)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_lsp::{CompletionItem, Diagnostic, DiagnosticSeverity, Position, Range as LspR};
    use lsp_types::TextEdit;

    fn tab_from(name: &str, text: &str) -> OpenTab {
        OpenTab::from_text(&PathBuf::from(format!("/x/{name}")), text, TokenPalette::forge_dark())
    }

    #[test]
    fn open_dedupes_and_close_index() {
        let mut e = EditorState::new(TokenPalette::forge_dark());
        e.push_tab(tab_from("a.cpp", "int a;\n"));
        e.push_tab(tab_from("b.cpp", "int b;\n"));
        assert_eq!(e.tabs.len(), 2);
        assert_eq!(e.active, Some(1));
        e.push_tab(tab_from("a.cpp", "int a;\n")); // dedupe → switch
        assert_eq!(e.tabs.len(), 2);
        assert_eq!(e.active, Some(0));
        e.close(0);
        assert_eq!(e.active, Some(0));
        assert_eq!(e.active_tab().unwrap().name, "b.cpp");
    }

    #[test]
    fn buffer_backed_tab_line_access() {
        let t = tab_from("a.cpp", "int x = 1;\n// c\n");
        assert_eq!(t.line_count(), 3);
        assert_eq!(t.line(0), "int x = 1;");
        assert_eq!(t.line(1), "// c");
        assert!(t.span_count() > 0);
    }

    #[test]
    fn debounce_fires_once_after_delay() {
        let mut d = Debounce::new(300);
        assert!(!d.is_pending());
        d.arm(1000);
        assert!(d.is_pending());
        assert!(!d.poll(1200)); // not yet
        assert!(!d.poll(1299));
        assert!(d.poll(1300)); // fires
        assert!(!d.poll(1400)); // consumed
        assert!(!d.is_pending());
        // Re-arming (typing burst) pushes the deadline out.
        d.arm(2000);
        d.arm(2100);
        assert!(!d.poll(2300));
        assert!(d.poll(2400));
    }

    #[test]
    fn on_edited_rehighlights_and_arms() {
        let mut t = tab_from("a.cpp", "int x;\n");
        // Type into the buffer, then run the eager trackers.
        t.buffer.type_char('y');
        t.on_edited(500);
        assert!(t.decorations_pending());
        // Flow fires at +300ms (=800), symbols at +800ms (=1300), sizes at
        // +1000ms (=1500).
        assert!(!t.poll_decorations(700)); // none due yet
        assert!(t.poll_decorations(800)); // flow due
        assert!(t.decorations_pending()); // struct + sizes still pending
        assert!(t.poll_decorations(1300)); // struct due
        assert!(t.poll_decorations(1500)); // sizes due
        assert!(!t.decorations_pending());
    }

    #[test]
    fn px_to_col_rounds_and_clamps() {
        // char_w = 8: x=11 → col 1 (round 1.375), x=13 → col 2 (round 1.625).
        assert_eq!(px_to_col(0.0, 8.0, 10), 0);
        assert_eq!(px_to_col(11.0, 8.0, 10), 1);
        assert_eq!(px_to_col(13.0, 8.0, 10), 2);
        assert_eq!(px_to_col(999.0, 8.0, 4), 4); // clamp to max_col
        assert_eq!(px_to_col(-5.0, 8.0, 10), 0);
        assert_eq!(col_to_px(3, 8.0), 24.0);
    }

    #[test]
    fn word_range_double_click() {
        let line = "foo_bar.baz(qux)";
        assert_eq!(word_range_in_line(line, 0), 0..7); // foo_bar
        assert_eq!(word_range_in_line(line, 4), 0..7);
        assert_eq!(word_range_in_line(line, 8), 8..11); // baz
        assert_eq!(word_range_in_line(line, 7), 7..8); // '.' → single char
        assert_eq!(word_range_in_line(line, 12), 12..15); // qux
    }

    #[test]
    fn ident_range_before_caret() {
        let buf = Buffer::from_text("  foo.bar");
        // caret at end (after "bar")
        let r = ident_range_before(&buf, buf.len_bytes());
        assert_eq!(&"  foo.bar"[r.clone()], "bar");
        // caret right after "foo"
        let r2 = ident_range_before(&buf, 5);
        assert_eq!(&"  foo.bar"[r2], "foo");
    }

    #[test]
    fn diag_span_single_and_multiline() {
        // Single-line: row 2, cols 4..9.
        assert_eq!(diag_span_on_row((2, 4), (2, 9), 2, 20), Some((4, 9)));
        // Row outside range.
        assert_eq!(diag_span_on_row((2, 4), (2, 9), 1, 20), None);
        assert_eq!(diag_span_on_row((2, 4), (2, 9), 3, 20), None);
        // Multi-line 1:3 .. 3:5 over a 12-col middle line.
        assert_eq!(diag_span_on_row((1, 3), (3, 5), 1, 12), Some((3, 12))); // first: to EOL
        assert_eq!(diag_span_on_row((1, 3), (3, 5), 2, 12), Some((0, 12))); // interior: whole
        assert_eq!(diag_span_on_row((1, 3), (3, 5), 3, 12), Some((0, 5))); // last: to end col
    }

    #[test]
    fn diagnostic_underlines_bytes() {
        let buf = Buffer::from_text("int x = 1;\nlong yy = 2;\n");
        // Diagnostic on row 1, cols 5..7 (the "yy").
        let d = Diagnostic {
            range: LspR {
                start: Position::new(1, 5),
                end: Position::new(1, 7),
            },
            severity: Some(DiagnosticSeverity::ERROR),
            ..Default::default()
        };
        let runs = diagnostic_underlines_for_row(&[d], 1, &buf, 0xff0000, 0xffaa00, 0x0000ff);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, 5..7);
        assert_eq!(runs[0].1, 0xff0000);
    }

    #[test]
    fn diagnostic_counts_by_severity() {
        let mk = |s| Diagnostic {
            severity: Some(s),
            ..Default::default()
        };
        let ds = vec![
            mk(DiagnosticSeverity::ERROR),
            mk(DiagnosticSeverity::ERROR),
            mk(DiagnosticSeverity::WARNING),
            mk(DiagnosticSeverity::INFORMATION),
        ];
        assert_eq!(diagnostic_counts(&ds), (2, 1, 1));
    }

    #[test]
    fn completion_filter_prefix() {
        let mk = |l: &str| CompletionItem {
            label: l.to_string(),
            ..Default::default()
        };
        let items = vec![mk("push_back"), mk("pop_back"), mk("push_front"), mk("size")];
        let f = completion_filter(&items, "pu");
        assert_eq!(f, vec![0, 2]); // push_back, push_front
        assert_eq!(completion_filter(&items, "").len(), 4);
        assert_eq!(completion_filter(&items, "zzz").len(), 0);
    }

    #[test]
    fn completion_edit_text_edit_range() {
        // Buffer: "  v.pu" — caret after "pu"; a textEdit replacing cols 4..6 of
        // row 0 with "push_back".
        let buf = Buffer::from_text("  v.pu");
        let item = CompletionItem {
            label: "push_back".into(),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: LspR {
                    start: Position::new(0, 4),
                    end: Position::new(0, 6),
                },
                new_text: "push_back".into(),
            })),
            ..Default::default()
        };
        let (range, text) = completion_edit(&item, &buf, 4..6);
        assert_eq!(range, 4..6);
        assert_eq!(text, "push_back");
    }

    #[test]
    fn completion_edit_insert_text_fallback() {
        let buf = Buffer::from_text("v.pu");
        let item = CompletionItem {
            label: "push_back".into(),
            insert_text: Some("push_back".into()),
            ..Default::default()
        };
        let (range, text) = completion_edit(&item, &buf, 2..4);
        assert_eq!(range, 2..4); // replaces the typed "pu"
        assert_eq!(text, "push_back");
    }

    #[test]
    fn utf16_doc_conversions_roundtrip() {
        // Mixed BMP + astral: "aü\n🎉b" — ü is 1 utf16 unit, 🎉 is 2.
        let buf = Buffer::from_text("aü\n🎉b");
        // Byte offsets that fall on char boundaries: a=0, ü=1(2 bytes), \n=3,
        // 🎉=4(4 bytes), b=8, end=9.
        for byte in [0usize, 1, 3, 4, 8, 9] {
            let u = byte_to_doc_utf16(&buf, byte);
            assert_eq!(doc_utf16_to_byte(&buf, u), byte, "byte {byte}");
        }
        // "aü" = a(1) ü(1) = 2 utf16, newline = 1 → row1 starts at utf16 3.
        let row1_start = buf.point_to_offset(Point::new(1, 0));
        assert_eq!(byte_to_doc_utf16(&buf, row1_start), 3);
    }

    #[test]
    fn selection_on_line_clips() {
        // Buffer rows: "abcd\nefgh\n". Row 0 bytes 0..4, row 1 bytes 5..9.
        // Selection bytes 2..7 (spans the newline).
        let sel = Selection::new(2, 7);
        assert_eq!(selection_on_line(sel, 0, 4), Some(2..4)); // row 0: cols 2..4
        assert_eq!(selection_on_line(sel, 5, 9), Some(0..2)); // row 1: cols 0..2
        // A row entirely outside.
        assert_eq!(selection_on_line(sel, 10, 14), None);
    }

    #[test]
    fn merge_line_styles_flattens_overlap() {
        // len 6, syntax color on 0..3, selection on 2..5, underline on 4..6.
        let syntax = vec![Span {
            start: 0,
            end: 3,
            color: 0x111111,
        }];
        let runs = merge_line_styles(6, &syntax, Some(2..5), &[(4..6, 0xff0000)], None);
        // Expected boundaries: 0..2 (color), 2..3 (color+sel), 3..4 (sel),
        // 4..5 (sel+underline), 5..6 (underline).
        assert_eq!(runs.len(), 5);
        assert_eq!(runs[0].0, 0..2);
        assert_eq!(runs[0].1.color, Some(0x111111));
        assert!(!runs[0].1.selected);
        assert_eq!(runs[1].0, 2..3);
        assert!(runs[1].1.selected && runs[1].1.color == Some(0x111111));
        assert_eq!(runs[3].0, 4..5);
        assert!(runs[3].1.selected && runs[3].1.underline == Some(0xff0000));
    }
}
