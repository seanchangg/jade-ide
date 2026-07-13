//! Small process helpers. `std::process` has no built-in timeout, so we poll
//! `try_wait` (the `execSync`/`execFile` timeouts in build-runner.ts are the
//! contract we're honoring).

use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Run a command capturing stdout/stderr, killing it (and returning `None`) if
/// it outruns `timeout`. Used for `atos`, `gcov`, and the dylib compiles.
pub fn output_with_timeout(mut cmd: Command, timeout: Duration) -> Option<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Like [`output_with_timeout`] but feeds `input` to stdin first — the `c++filt`
/// demangle pass (build-runner.ts `demangleCpp`, 5s timeout).
pub fn output_with_stdin_timeout(
    mut cmd: Command,
    input: &str,
    timeout: Duration,
) -> Option<Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    // Write stdin from a scratch thread so a full stdout pipe can't deadlock us.
    if let Some(mut stdin) = child.stdin.take() {
        let owned = input.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(owned.as_bytes());
        });
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}
