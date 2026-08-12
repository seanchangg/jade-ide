//! Integration tests for `jade-term`.
//!
//! These run a real PTY + shell — no GUI. To keep them deterministic and
//! independent of the developer's login shell, tests spawn `/bin/sh`
//! explicitly via `create_with_shell`. Production code still defaults to
//! `$SHELL`/`/bin/zsh` (see `TermManager::create`).

use std::path::Path;
use std::time::{Duration, Instant};

use jade_term::{Color, TermEvent, TermManager};
use tokio::sync::mpsc::UnboundedReceiver;

const SH: &str = "/bin/sh";

/// Poll `snapshot` until `pred` holds or `timeout` elapses.
async fn wait_for_snapshot<F>(
    manager: &TermManager,
    id: u32,
    timeout: Duration,
    mut pred: F,
) -> bool
where
    F: FnMut(&jade_term::GridSnapshot) -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(snap) = manager.snapshot(id) {
            if pred(&snap) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Await a specific `Exited` event (draining Damaged events) or time out.
async fn wait_for_exit(
    events: &mut UnboundedReceiver<TermEvent>,
    id: u32,
    timeout: Duration,
) -> Option<Option<i32>> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(TermEvent::Exited { id: eid, code })) if eid == id => return Some(code),
            Ok(Some(_)) => continue, // Damaged / other-instance events
            Ok(None) => return None,
            Err(_) => return None, // timed out
        }
    }
}

/// (a) Spawn with cwd=/tmp, write `printf 'hello-jade-term\n'`, and poll
/// snapshots until the text appears in the grid.
#[tokio::test]
async fn spawn_and_capture_output() {
    let manager = TermManager::new();
    let id = manager
        .create_with_shell(Path::new("/tmp"), Path::new(SH))
        .expect("PTY should be available on this machine");
    assert_eq!(id, 1, "ids start at 1");

    manager.write(id, b"printf 'hello-jade-term\\n'\n");

    let found = wait_for_snapshot(&manager, id, Duration::from_secs(10), |snap| {
        snap.contains_text("hello-jade-term")
    })
    .await;
    assert!(found, "expected 'hello-jade-term' to appear in the grid");

    manager.destroy(id);
}

/// (b) Resize 80x24 -> 120x40 is reflected in snapshot dimensions.
#[tokio::test]
async fn resize_reflected_in_snapshot() {
    let manager = TermManager::new();
    let id = manager
        .create_with_shell(Path::new("/tmp"), Path::new(SH))
        .expect("PTY available");

    let snap = manager.snapshot(id).expect("snapshot");
    assert_eq!((snap.cols, snap.rows), (80, 24), "initial geometry is 80x24");
    assert_eq!(snap.cells.len(), 24);
    assert!(snap.cells.iter().all(|r| r.len() == 80));

    manager.resize(id, 120, 40);

    // Resize is synchronous on the grid, but read via a short poll to be safe.
    let ok = wait_for_snapshot(&manager, id, Duration::from_secs(5), |snap| {
        snap.cols == 120 && snap.rows == 40
    })
    .await;
    assert!(ok, "snapshot should reflect 120x40");

    let snap = manager.snapshot(id).unwrap();
    assert_eq!(snap.cells.len(), 40);
    assert!(snap.cells.iter().all(|r| r.len() == 120));

    manager.destroy(id);
}

/// (c) `exit\n` produces an Exited event with code 0.
#[tokio::test]
async fn exit_reports_code_zero() {
    let manager = TermManager::new();
    let mut events = manager.take_events().expect("event stream");
    let id = manager
        .create_with_shell(Path::new("/tmp"), Path::new(SH))
        .expect("PTY available");

    manager.write(id, b"exit 0\n");

    let code = wait_for_exit(&mut events, id, Duration::from_secs(10)).await;
    assert_eq!(code, Some(Some(0)), "shell exit 0 should report code 0");
}

/// (d) Two instances get ids 1 and 2; destroy(1) leaves 2 usable.
#[tokio::test]
async fn two_instances_ids_and_destroy() {
    let manager = TermManager::new();
    let id1 = manager
        .create_with_shell(Path::new("/tmp"), Path::new(SH))
        .expect("PTY available");
    let id2 = manager
        .create_with_shell(Path::new("/tmp"), Path::new(SH))
        .expect("PTY available");
    assert_eq!(id1, 1);
    assert_eq!(id2, 2);

    manager.destroy(id1);
    assert!(!manager.contains(id1), "instance 1 destroyed");
    assert!(manager.contains(id2), "instance 2 still alive");

    // Instance 2 is still usable.
    manager.write(id2, b"printf 'second-term-ok\\n'\n");
    let found = wait_for_snapshot(&manager, id2, Duration::from_secs(10), |snap| {
        snap.contains_text("second-term-ok")
    })
    .await;
    assert!(found, "instance 2 should still produce output after destroy(1)");

    manager.destroy(id2);
}

/// (f) Terminal queries are answered: a DSR cursor-position query (`ESC[6n`)
/// must produce a reply on the PTY (`ESC[<row>;<col>R`), or interactive TUIs
/// (Ink/Claude Code, vim) hang or mis-layout. `dd` blocks until the 6 reply
/// bytes arrive, so `DSR-OK` only prints if the answerback path works.
#[tokio::test]
async fn dsr_cursor_position_query_is_answered() {
    let manager = TermManager::new();
    let id = manager
        .create_with_shell(Path::new("/tmp"), Path::new(SH))
        .expect("PTY available");

    // Raw-ish mode (`-icanon`) like a real TUI, or the line discipline holds
    // the newline-less reply back from dd. The echoed command line contains
    // "DSR-'OK'" (with quotes), so matching on DSR-OK only hits echo's output.
    manager.write(
        id,
        b"stty -icanon -echo; printf '\\033[6n'; dd bs=1 count=6 >/dev/null 2>&1; stty icanon echo; echo DSR-'OK'\n",
    );

    let found = wait_for_snapshot(&manager, id, Duration::from_secs(10), |snap| {
        snap.contains_text("DSR-OK")
    })
    .await;
    assert!(found, "DSR query should be answered so dd unblocks and DSR-OK prints");

    manager.destroy(id);
}

/// (e) ANSI color: red output yields a cell with fg = Named(1) / Indexed(1).
///
/// We use octal `\033` for the ESC byte, which every POSIX `printf` (and
/// `/bin/sh`) handles identically — the task's `\x1b` is bash-specific.
#[tokio::test]
async fn ansi_red_foreground() {
    let manager = TermManager::new();
    let id = manager
        .create_with_shell(Path::new("/tmp"), Path::new(SH))
        .expect("PTY available");

    manager.write(id, b"printf '\\033[31mred\\033[0m\\n'\n");

    // Find a cell whose foreground is the ANSI "red" slot. The echoed command
    // line also contains "red" but with default fg, so we key on the color.
    let is_red = |c: &Color| matches!(c, Color::Named(1) | Color::Indexed(1));
    let found = wait_for_snapshot(&manager, id, Duration::from_secs(10), |snap| {
        let rows = snap.cells.iter().chain(snap.scrollback.iter());
        rows.flat_map(|r| r.iter()).any(|cell| cell.ch == 'r' && is_red(&cell.fg))
    })
    .await;
    assert!(found, "expected a red-foreground 'r' cell from the ANSI sequence");

    manager.destroy(id);
}
