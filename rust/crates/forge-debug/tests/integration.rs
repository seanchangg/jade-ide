//! End-to-end integration test driving a REAL `lldb` against a tiny C++ binary
//! we compile with `clang++ -O0 -g`. Exercises the full prompt protocol:
//! start → stop at breakpoint (locals populated) → step-over (line advances) →
//! continue → exit(0).
//!
//! Skips gracefully (returns, prints a note) when `lldb` or `clang++` is
//! unavailable, so `cargo test` stays green on machines without a toolchain.

use std::process::Command;

use forge_debug::{Breakpoint, DebugEvent, LldbDriver};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{timeout, Duration};

/// The debuggee. `compute` has a breakpoint-friendly line where `a` is live.
const PROGRAM: &str = r#"int compute(int seed) {
  int a = seed + 1;
  int b = a * 2;      // BREAKPOINT (line 3)
  return b;
}

int main() {
  int r = compute(40);
  return r > 0 ? 0 : 1;
}
"#;
const BREAK_LINE: u32 = 3;

fn tool_available(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Await the next `Stopped` or `Exited`, discarding `Output` passthrough.
async fn next_significant(rx: &mut UnboundedReceiver<DebugEvent>) -> DebugEvent {
    loop {
        let ev = timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("timed out waiting for a debug event")
            .expect("event channel closed unexpectedly");
        if !matches!(ev, DebugEvent::Output(_)) {
            return ev;
        }
    }
}

#[tokio::test]
async fn lldb_end_to_end_breakpoint_step_continue_exit() {
    if !tool_available("lldb") || !tool_available("clang++") {
        eprintln!("skipping: lldb and/or clang++ not available on PATH");
        return;
    }

    // ── compile the debuggee into a unique temp dir ──
    let dir = std::env::temp_dir().join(format!("forge-debug-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("prog.cpp");
    let exe = dir.join("prog");
    std::fs::write(&src, PROGRAM).unwrap();

    let status = Command::new("clang++")
        .args(["-O0", "-g", "-std=c++17", "-o"])
        .arg(&exe)
        .arg(&src)
        .status()
        .expect("failed to invoke clang++");
    assert!(status.success(), "clang++ failed to compile the debuggee");

    // ── drive lldb ──
    let (mut driver, mut events) = LldbDriver::new();
    driver
        .start(
            exe.to_str().unwrap(),
            dir.to_str().unwrap(),
            &[Breakpoint::new("prog.cpp", BREAK_LINE)],
            &[], // env_vars seam: none for this test
        )
        .await
        .expect("driver failed to start lldb");

    // 1. Stopped at the breakpoint with `a` among the locals.
    let stopped = next_significant(&mut events).await;
    let (reason, file, line, locals) = match stopped {
        DebugEvent::Stopped {
            reason,
            file,
            line,
            locals,
            ..
        } => (reason, file, line, locals),
        other => panic!("expected Stopped at breakpoint, got {other:?}"),
    };
    assert!(
        reason.contains("breakpoint"),
        "stop reason should mention a breakpoint, got {reason:?}"
    );
    assert!(
        file.ends_with("prog.cpp"),
        "stop file should be prog.cpp, got {file:?}"
    );
    assert_eq!(line, BREAK_LINE, "should stop at the breakpoint line");
    let a = locals
        .iter()
        .find(|v| v.name == "a")
        .expect("local `a` should be present at the breakpoint");
    assert_eq!(a.value, "41", "a = seed(40) + 1");

    // 2. Step over: the current line advances past the breakpoint.
    driver.step_over().await;
    let after_step = next_significant(&mut events).await;
    match after_step {
        DebugEvent::Stopped { line: new_line, .. } => {
            assert!(
                new_line > BREAK_LINE,
                "step-over should advance the line (was {BREAK_LINE}, now {new_line})"
            );
        }
        other => panic!("expected Stopped after step-over, got {other:?}"),
    }

    // 3. Continue to completion → clean exit.
    driver.continue_().await;
    let exit = next_significant(&mut events).await;
    match exit {
        DebugEvent::Exited(code) => assert_eq!(code, 0, "program should exit 0"),
        other => panic!("expected Exited(0), got {other:?}"),
    }

    driver.stop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A driver with no live session ignores control calls instead of panicking
/// (the TS `if (!this.proc || !this.ready) return;` guards). No lldb needed.
#[tokio::test]
async fn control_methods_noop_without_session() {
    let (mut driver, _events) = LldbDriver::new();
    driver.set_breakpoint("x.cpp", 1).await;
    driver.remove_breakpoint("x.cpp", 1).await;
    driver.continue_().await;
    driver.step_over().await;
    driver.step_into().await;
    driver.step_out().await;
    assert!(driver.get_var_children("foo").await.is_empty());
    driver.stop().await; // idempotent no-op
}
