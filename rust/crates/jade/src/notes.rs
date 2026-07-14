//! Sticky notes (§4.9) — model, per-workspace persistence, and the markdown
//! `[ ]`/`[x]` ↔ `☐`/`☑` checkbox logic. Pure + unit-tested; `app.rs`/`code_view.rs`
//! own the overlay rendering and the drag/resize/edit mouse+key wiring.
//!
//! ## Persistence & migration
//! Notes live in the Electron location — `<workspaceRoot>/.forge/workspace.json`,
//! under `ui.stickyNotes` (the renderer bundles UI state under a single `ui` key,
//! `state.ts:116`). [`load`] reads that path so **existing Electron notes migrate
//! unchanged**; [`save`] round-trips through the file preserving every other `ui`
//! key the Electron app wrote (open tabs, panel visibility, …).
//!
//! The serialized shape matches `shared/types.ts:118-129` (`camelCase`), so the
//! same JSON is interchangeable between the two apps.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Default new-note size (§4.9 / limits table).
pub const DEFAULT_W: f32 = 220.0;
pub const DEFAULT_H: f32 = 160.0;
/// Minimum size while resizing.
pub const MIN_W: f32 = 160.0;
pub const MIN_H: f32 = 100.0;
/// Content-save debounce (TS `sticky-notes.ts:153`).
pub const SAVE_DEBOUNCE_MS: u64 = 500;

/// One sticky note (`shared/types.ts:118-129`). `content` is markdown-ish text
/// with `[ ]`/`[x]` checkboxes; timestamps are `Date.now()` epoch-ms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StickyNote {
    pub id: String,
    pub file_path: String,
    pub anchor_line: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub content: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl StickyNote {
    /// Build a fresh note anchored on `line` of `file_path`, at mouse `(x, y)`.
    pub fn new(file_path: impl Into<String>, line: usize, x: f32, y: f32) -> StickyNote {
        let now = now_ms();
        StickyNote {
            id: new_id(now),
            file_path: file_path.into(),
            anchor_line: line,
            x,
            y,
            width: DEFAULT_W,
            height: DEFAULT_H,
            content: String::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = now_ms();
    }
}

/// `Date.now()` epoch-ms.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A process-unique id (`note-<ms>-<seq>`) — mirrors the TS `note-<ts>-<rand>`
/// without needing an RNG; the atomic seq guarantees uniqueness within a burst.
fn new_id(now: i64) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("note-{now}-{n:04x}")
}

// ── Per-workspace persistence ────────────────────────────────────────────────

/// The `.forge/workspace.json` path for a workspace root.
pub fn workspace_json_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".forge").join("workspace.json")
}

/// Load the sticky notes for a workspace (`ui.stickyNotes`), or empty on any
/// error / absence. Electron-written notes load verbatim.
pub fn load(workspace_root: &Path) -> Vec<StickyNote> {
    load_from(&workspace_json_path(workspace_root))
}

/// Load from an explicit `workspace.json` path (test seam).
pub fn load_from(path: &Path) -> Vec<StickyNote> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else {
        return Vec::new();
    };
    v.get("ui")
        .and_then(|ui| ui.get("stickyNotes"))
        .and_then(|n| serde_json::from_value::<Vec<StickyNote>>(n.clone()).ok())
        .unwrap_or_default()
}

/// Persist `notes` under `ui.stickyNotes`, **preserving** every other key already
/// present in the file (other `ui` state + any top-level keys). Errors swallowed.
pub fn save(workspace_root: &Path, notes: &[StickyNote]) {
    save_to(&workspace_json_path(workspace_root), notes);
}

/// Save to an explicit path (test seam).
pub fn save_to(path: &Path, notes: &[StickyNote]) {
    // Read-modify-write so we don't clobber other UI state the Electron app owns.
    let mut root: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let ui = obj
        .entry("ui")
        .or_insert_with(|| serde_json::json!({}));
    if !ui.is_object() {
        *ui = serde_json::json!({});
    }
    ui.as_object_mut().unwrap().insert(
        "stickyNotes".to_string(),
        serde_json::to_value(notes).unwrap_or(serde_json::Value::Null),
    );
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(&root) {
        let _ = std::fs::write(path, s);
    }
}

// ── Checkbox markdown ↔ display ──────────────────────────────────────────────

/// A render segment of a note line: plain text, or a checkbox with its **global**
/// ordinal (across the whole note) so a click can toggle the right one.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Text(String),
    Checkbox { checked: bool, index: usize },
}

/// A checkbox token starting at byte `i` in `bytes`: `(checked, token_len)`.
/// Recognizes `[x]`/`[X]` (checked), `[ ]` and `[]` (unchecked).
fn checkbox_at(bytes: &[u8], i: usize) -> Option<(bool, usize)> {
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    match bytes.get(i + 1) {
        Some(&b']') => Some((false, 2)),                       // []
        Some(&b' ') if bytes.get(i + 2) == Some(&b']') => Some((false, 3)), // [ ]
        Some(&c) if (c == b'x' || c == b'X') && bytes.get(i + 2) == Some(&b']') => {
            Some((true, 3)) // [x]
        }
        _ => None,
    }
}

/// Split note `content` into per-line render segments, assigning each checkbox a
/// global ordinal. Empty lines yield an empty segment vec.
pub fn render_segments(content: &str) -> Vec<Vec<Segment>> {
    let mut checkbox_index = 0usize;
    content
        .split('\n')
        .map(|line| {
            let bytes = line.as_bytes();
            let mut segs: Vec<Segment> = Vec::new();
            let mut text_start = 0usize;
            let mut i = 0usize;
            while i < bytes.len() {
                if let Some((checked, len)) = checkbox_at(bytes, i) {
                    if i > text_start {
                        segs.push(Segment::Text(line[text_start..i].to_string()));
                    }
                    segs.push(Segment::Checkbox {
                        checked,
                        index: checkbox_index,
                    });
                    checkbox_index += 1;
                    i += len;
                    text_start = i;
                } else {
                    i += 1;
                }
            }
            if text_start < bytes.len() {
                segs.push(Segment::Text(line[text_start..].to_string()));
            }
            segs
        })
        .collect()
}

/// Flip the `target`-th checkbox (global ordinal) in `content`, returning the new
/// markdown. Toggling normalizes tokens to `[x]`/`[ ]` (TS `htmlToMarkdown`).
pub fn toggle_checkbox(content: &str, target: usize) -> String {
    let bytes = content.as_bytes();
    let mut out = String::with_capacity(content.len());
    let mut idx = 0usize;
    let mut i = 0usize;
    let mut last = 0usize;
    while i < bytes.len() {
        if let Some((checked, len)) = checkbox_at(bytes, i) {
            out.push_str(&content[last..i]);
            let now = if idx == target { !checked } else { checked };
            out.push_str(if now { "[x]" } else { "[ ]" });
            idx += 1;
            i += len;
            last = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&content[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_note_defaults() {
        let n = StickyNote::new("/x/a.cpp", 12, 30.0, 40.0);
        assert_eq!(n.anchor_line, 12);
        assert_eq!((n.width, n.height), (DEFAULT_W, DEFAULT_H));
        assert!(n.content.is_empty());
        assert!(n.id.starts_with("note-"));
    }

    #[test]
    fn ids_are_unique() {
        let a = StickyNote::new("/x", 1, 0.0, 0.0);
        let b = StickyNote::new("/x", 1, 0.0, 0.0);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn render_segments_marks_checkboxes_with_global_index() {
        let segs = render_segments("todo [ ] buy milk\n[x] done [ ] later");
        assert_eq!(
            segs[0],
            vec![
                Segment::Text("todo ".into()),
                Segment::Checkbox { checked: false, index: 0 },
                Segment::Text(" buy milk".into()),
            ]
        );
        assert_eq!(
            segs[1],
            vec![
                Segment::Checkbox { checked: true, index: 1 },
                Segment::Text(" done ".into()),
                Segment::Checkbox { checked: false, index: 2 },
                Segment::Text(" later".into()),
            ]
        );
    }

    #[test]
    fn render_segments_handles_empty_bracket_form() {
        let segs = render_segments("[] pending");
        assert_eq!(
            segs[0],
            vec![
                Segment::Checkbox { checked: false, index: 0 },
                Segment::Text(" pending".into()),
            ]
        );
    }

    #[test]
    fn toggle_flips_only_the_target_and_normalizes() {
        let c = "[ ] a\n[x] b\n[] c";
        // Toggle #0 unchecked→checked; normalize the "[]" (#2) stays but as "[ ]".
        let t0 = toggle_checkbox(c, 0);
        assert_eq!(t0, "[x] a\n[x] b\n[ ] c");
        // Toggle #1 checked→unchecked.
        let t1 = toggle_checkbox(c, 1);
        assert_eq!(t1, "[ ] a\n[ ] b\n[ ] c");
        // Out-of-range target changes nothing but still normalizes tokens.
        let t9 = toggle_checkbox(c, 9);
        assert_eq!(t9, "[ ] a\n[x] b\n[ ] c");
    }

    #[test]
    fn save_load_roundtrip_under_ui_key() {
        let dir = std::env::temp_dir().join(format!("jade-notes-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("ws");
        let mut n = StickyNote::new("/ws/a.cpp", 5, 100.0, 120.0);
        n.content = "[ ] task".to_string();
        save(&root, &[n.clone()]);
        let loaded = load(&root);
        assert_eq!(loaded, vec![n]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_preserves_other_ui_keys() {
        let dir = std::env::temp_dir().join(format!("jade-notes-pres-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(".forge").join("workspace.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Simulate an Electron-written file with other ui state.
        std::fs::write(
            &path,
            r#"{ "ui": { "terminalVisible": true, "activeTabIndex": 2, "stickyNotes": [] } }"#,
        )
        .unwrap();
        let n = StickyNote::new("/x", 1, 0.0, 0.0);
        save_to(&path, &[n.clone()]);
        // Re-read raw and confirm the sibling keys survived.
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["ui"]["terminalVisible"], serde_json::json!(true));
        assert_eq!(v["ui"]["activeTabIndex"], serde_json::json!(2));
        assert_eq!(load_from(&path), vec![n]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_existing_electron_notes() {
        // A fixture written exactly as the Electron app would (camelCase, ui key).
        let dir = std::env::temp_dir().join(format!("jade-notes-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(".forge").join("workspace.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
              "ui": {
                "stickyNotes": [
                  {
                    "id": "note-1700000000000-abc123",
                    "filePath": "/proj/main.cpp",
                    "anchorLine": 42,
                    "x": 300, "y": 200, "width": 220, "height": 160,
                    "content": "[x] ported\n[ ] test",
                    "createdAt": 1700000000000,
                    "updatedAt": 1700000000500
                  }
                ]
              }
            }"#,
        )
        .unwrap();
        let notes = load_from(&path);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, "note-1700000000000-abc123");
        assert_eq!(notes[0].anchor_line, 42);
        assert_eq!(notes[0].content, "[x] ported\n[ ] test");
        assert_eq!(notes[0].created_at, 1700000000000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
