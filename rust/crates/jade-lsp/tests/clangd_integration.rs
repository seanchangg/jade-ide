//! Integration tests against the REAL clangd (Apple clangd 17 via `xcrun`).
//!
//! Skips gracefully (prints and returns) when `xcrun -f clangd` fails, so CI on
//! a machine without the Xcode toolchain stays green. All subtests are
//! serialized inside a single `#[tokio::test]` so exactly one clangd spawns.
//!
//! Timings are deliberately generous: clangd does background indexing and its
//! first completion can take several seconds after spawn.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jade_lsp::{DidChange, LspClient, LspEvent, Position, Range, TextDocumentSyncKind, Utf16RangeEdit};
use tokio::sync::mpsc::UnboundedReceiver;

/// Resolve the real clangd via `xcrun -f clangd` and prepend its directory to
/// `PATH` so `Command::new("clangd")` inside the client picks it up. Returns
/// `false` (test should skip) if xcrun can't find clangd.
fn ensure_clangd_on_path() -> bool {
    let out = std::process::Command::new("xcrun").args(["-f", "clangd"]).output();
    let Ok(out) = out else {
        eprintln!("SKIP: `xcrun` not runnable");
        return false;
    };
    if !out.status.success() {
        eprintln!("SKIP: `xcrun -f clangd` failed");
        return false;
    }
    let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dir = Path::new(&full).parent().map(|p| p.to_path_buf());
    if let Some(dir) = dir {
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", dir.display(), old));
        eprintln!("using clangd: {full}");
        true
    } else {
        eprintln!("SKIP: could not derive clangd dir from {full}");
        false
    }
}

/// The line index (0-based) of the first line containing `needle`.
fn line_of(text: &str, needle: &str) -> u32 {
    text.lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line containing {needle:?}")) as u32
}

/// Await the next diagnostics event for `path` satisfying `pred`, within
/// `timeout`. Returns the matching diagnostics count, or `None` on timeout.
async fn await_diagnostics(
    events: &mut UnboundedReceiver<LspEvent>,
    path: &Path,
    timeout: Duration,
    pred: impl Fn(usize) -> bool,
) -> Option<usize> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(LspEvent::Diagnostics { path: p, diagnostics })) => {
                if p == path && pred(diagnostics.len()) {
                    return Some(diagnostics.len());
                }
            }
            Ok(Some(_)) => continue, // Ready / other
            Ok(None) => return None, // channel closed
            Err(_) => return None,   // timed out
        }
    }
}

#[tokio::test]
async fn clangd_end_to_end() {
    if !ensure_clangd_on_path() {
        return;
    }

    // ── temp project: main.cpp with an intentional error + compile_commands ──
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let main_cpp: PathBuf = root.join("main.cpp");

    let source = "\
struct Widget {
    int width;
    int height;
};

int area(const Widget& w) {
    return w.width * w.height;
}

int main() {
    Widget box;
    box.width = 3;
    box.height = 4;
    int x = \"s\";
    return area(box);
}
";
    std::fs::write(&main_cpp, source).unwrap();

    // compile_commands.json makes clangd's compile flags deterministic.
    let cc = format!(
        r#"[{{"directory":"{d}","file":"{f}","command":"clang++ -std=c++17 -c main.cpp"}}]"#,
        d = root.display(),
        f = main_cpp.display()
    );
    std::fs::write(root.join("compile_commands.json"), cc).unwrap();

    // ── initialize ──
    let mut handle = LspClient::initialize(&root, None)
        .await
        .expect("clangd initialize");

    // Capability negotiation: clangd 17 advertises incremental sync.
    let sync = handle.sync_kind();
    eprintln!("negotiated TextDocumentSyncKind = {sync:?}");
    assert_eq!(
        sync,
        TextDocumentSyncKind::INCREMENTAL,
        "expected clangd to negotiate incremental sync"
    );

    let mut events = handle.take_events().expect("event receiver");

    // ── didOpen ──
    handle.did_open(&main_cpp, source, 1).expect("did_open");

    // (a) diagnostics arrive for the intentional error.
    let t0 = std::time::Instant::now();
    let count = await_diagnostics(&mut events, &main_cpp, Duration::from_secs(30), |n| n > 0)
        .await
        .expect("expected a non-empty diagnostics event for the error");
    eprintln!("(a) diagnostics: {count} diagnostic(s) in {:?}", t0.elapsed());
    assert!(count > 0);

    // (c) hover over the `area` function name at its definition returns content.
    let area_def_line = line_of(source, "int area(const Widget& w)");
    let t1 = std::time::Instant::now();
    let hover = poll_hover(&handle, &main_cpp, Position::new(area_def_line, 4), Duration::from_secs(20)).await;
    eprintln!("(c) hover resolved in {:?}", t1.elapsed());
    assert!(hover, "expected hover content over `area`");

    // (b) member completion after `box.` returns non-empty items.
    let box_line = line_of(source, "box.width = 3;");
    // `    box.width` → dot at col 7, completion just after it at col 8.
    let t2 = std::time::Instant::now();
    let items = poll_completion(&handle, &main_cpp, Position::new(box_line, 8), Duration::from_secs(30)).await;
    eprintln!("(b) completion: {items} item(s) in {:?}", t2.elapsed());
    assert!(items > 0, "expected non-empty member completion after `box.`");

    // (d) definition from the `area(box)` call resolves to the def line.
    let call_line = line_of(source, "return area(box);");
    let call_col = source
        .lines()
        .nth(call_line as usize)
        .unwrap()
        .find("area(box)")
        .unwrap() as u32
        + 1; // land inside the identifier
    let t3 = std::time::Instant::now();
    let def = poll_definition(&handle, &main_cpp, Position::new(call_line, call_col), area_def_line, Duration::from_secs(20)).await;
    eprintln!("(d) definition resolved to line {area_def_line} in {:?}", t3.elapsed());
    assert!(def, "expected definition of `area` to resolve to its def line");

    // (e) incremental didChange: replace `"s"` with `0`, expect diagnostics to
    // clear (an empty diagnostics event).
    let err_line = line_of(source, "int x = \"s\";");
    let quote_col = source
        .lines()
        .nth(err_line as usize)
        .unwrap()
        .find('"')
        .unwrap() as u32;
    // `"s"` spans quote_col ..= quote_col + 2 (3 chars). Replace with `0`.
    let edit = Utf16RangeEdit {
        range: Range::new(
            Position::new(err_line, quote_col),
            Position::new(err_line, quote_col + 3),
        ),
        text: "0".to_string(),
    };
    handle
        .did_change(&main_cpp, DidChange::Incremental(vec![edit]), 2)
        .expect("incremental did_change");

    let t4 = std::time::Instant::now();
    let cleared = await_diagnostics(&mut events, &main_cpp, Duration::from_secs(30), |n| n == 0)
        .await
        .expect("expected an empty diagnostics event after the fix");
    eprintln!("(e) diagnostics cleared ({cleared} remaining) in {:?}", t4.elapsed());
    assert_eq!(cleared, 0);

    // ── graceful shutdown ──
    handle.shutdown().await;
    eprintln!("all subtests passed");
}

async fn poll_hover(
    handle: &jade_lsp::LspHandle,
    path: &Path,
    pos: Position,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(_)) = handle.hover(path, pos).await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

async fn poll_completion(
    handle: &jade_lsp::LspHandle,
    path: &Path,
    pos: Position,
    timeout: Duration,
) -> usize {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(items) = handle.completion(path, pos).await {
            if !items.is_empty() {
                return items.len();
            }
        }
        if std::time::Instant::now() >= deadline {
            return 0;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn poll_definition(
    handle: &jade_lsp::LspHandle,
    path: &Path,
    pos: Position,
    expect_line: u32,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(locs) = handle.definition(path, pos).await {
            if locs.iter().any(|l| l.range.start.line == expect_line) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}
