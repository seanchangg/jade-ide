//! The LLDB driver: an interactive `lldb <executable>` wrapper. Port of the
//! `LldbDriver` class in `src/main/debug-driver.ts` (lines 11-406).
//!
//! Architecture. The TS driver runs inside Electron's single-threaded event
//! loop, juggling stdout chunks, per-command promises and three timers
//! (150ms settle / 80ms async / 10s command timeout) by hand. We reproduce
//! that exactly with one dedicated tokio task per session (the "session
//! actor"), so all protocol state lives in a single owner with no locking:
//!
//!   - [`LldbDriver`]  — the handle. `start` spawns lldb, runs the synchronous
//!     handshake, then hands the ready [`Session`] off to the actor task.
//!   - control methods (`set_breakpoint`, `continue_`, `step_*`, `stop`,
//!     `get_var_children`) send a [`Request`] to the actor and await its
//!     oneshot reply — this is the tokio analogue of the TS `cmdQueue`, giving
//!     one in-flight operation at a time.
//!   - the actor's idle loop (`run`) owns stdout while no command is pending and
//!     implements the 80ms async-settle window for unsolicited stop/exit output.
//!
//! Timing constants (docs/jade-feature-inventory.md §8.4 / §10):
//!   settle 150ms · async 80ms · command timeout 10s · SIGKILL fallback 1s.

use std::process::Stdio;
use std::time::Duration;

use regex::Regex;
use std::sync::LazyLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep_until, Instant};

use crate::parse::{parse_backtrace, parse_variables};
use crate::types::{Breakpoint, DebugEvent, LocalVariable};

/// Custom prompt (`debug-driver.ts:8`). Distinct from lldb's default `(lldb)`
/// so we can recognize it unambiguously in the byte stream.
const PROMPT: &str = "JADE_LLDB> ";
/// A *trailing* prompt only counts as a command boundary when it sits at the
/// start of a line. This lldb (macOS Command Line Tools) echoes every command
/// and re-prints the prompt when it's ready for input, so a bare `PROMPT` match
/// would fire on the echoed prompt string inside `settings set prompt
/// "JADE_LLDB> "` or on a stale leading prompt, shifting every response by one
/// command. `\nJADE_LLDB> ` matches only a genuine end-of-output prompt.
const PROMPT_LINE: &str = "\nJADE_LLDB> ";
/// lldb's default prompt at line start — the banner is drained until this is
/// seen (or the custom prompt, if a previous session already switched it).
const LLDB_PROMPT_LINE: &str = "\n(lldb) ";

// ─── Timing constants (§8.4 / §10) ───
const SETTLE: Duration = Duration::from_millis(150);
const ASYNC_SETTLE: Duration = Duration::from_millis(80);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_FALLBACK: Duration = Duration::from_secs(1);
/// Bound on the initial banner wait (TS keys off the first `(lldb)` line via a
/// 100ms poke; we cap the read so a broken spawn can't hang `start` forever).
const PROMPT_WAIT: Duration = Duration::from_secs(10);

static EXIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Process \d+ exited with status = (-?\d+)").unwrap());
static STOP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"stop reason = (.+)").unwrap());
static LOC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"at\s+(\S+?):(\d+)").unwrap());

/// A control operation sent from an [`LldbDriver`] method to the session actor.
/// Each carries a oneshot the actor fires once the operation (including any
/// event emission) completes — serializing callers just like the TS `cmdQueue`.
enum Request {
    SetBreakpoint {
        file: String,
        line: u32,
        reply: oneshot::Sender<()>,
    },
    RemoveBreakpoint {
        file: String,
        line: u32,
        reply: oneshot::Sender<()>,
    },
    Continue(oneshot::Sender<()>),
    StepOver(oneshot::Sender<()>),
    StepInto(oneshot::Sender<()>),
    StepOut(oneshot::Sender<()>),
    GetVarChildren {
        expr: String,
        reply: oneshot::Sender<Vec<LocalVariable>>,
    },
    Stop(oneshot::Sender<()>),
}

/// Handle to the actor task of a live lldb session.
struct SessionHandle {
    req_tx: mpsc::Sender<Request>,
    task: tokio::task::JoinHandle<()>,
}

/// The LLDB driver. Holds at most one live session; `start` is idempotent and
/// replaces any prior one. Events reach the UI through the mpsc receiver
/// returned by [`LldbDriver::new`].
pub struct LldbDriver {
    events_tx: mpsc::UnboundedSender<DebugEvent>,
    session: Option<SessionHandle>,
}

impl LldbDriver {
    /// Create a driver and its event stream. The receiver yields
    /// [`DebugEvent`]s (`Output`, `Stopped`, `Exited`) for the lifetime of the
    /// driver across successive `start`/`stop` cycles.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<DebugEvent>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        (
            LldbDriver {
                events_tx,
                session: None,
            },
            events_rx,
        )
    }

    /// Start (or restart) a debug session. Port of `start`
    /// (`debug-driver.ts:32-98`): spawns `lldb <executable>` in `cwd`, waits for
    /// the banner, configures machine-parseable output, injects `env_vars` via
    /// `target.env-vars`, sets breakpoints, `run`s, and reports an immediate
    /// stop. Idempotent — a prior session is stopped first.
    ///
    /// `env_vars` is the Phase-3 seam (inventory §8.4): supply
    /// `JADE_TELEMETRY_SOCK` and the probe dylib (`DYLD_INSERT_LIBRARIES`)
    /// here. Empty = no `target.env-vars` command, exactly like the TS guard.
    pub async fn start(
        &mut self,
        executable: &str,
        cwd: &str,
        breakpoints: &[Breakpoint],
        env_vars: &[(String, String)],
    ) -> std::io::Result<()> {
        // Idempotent: stop a prior session before starting a new one.
        if self.session.is_some() {
            self.stop().await;
        }

        let mut child = Command::new("lldb")
            .arg(executable)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // stderr is forwarded verbatim as Output, never mixed into the command
        // buffer (debug-driver.ts:48-51).
        let err_tx = self.events_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = err_tx
                            .send(DebugEvent::Output(String::from_utf8_lossy(&chunk[..n]).into()));
                    }
                }
            }
        });

        let mut session = Session {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            buffer: String::new(),
            pending: Vec::new(),
            events: self.events_tx.clone(),
            ready: false,
            exit_sent: false,
        };

        // ── Handshake (runs to completion before the idle loop starts, so no
        //    concurrent stdout consumer can race the read loop). ──
        session.wait_for_prompt().await;
        session
            .send_command(&format!("settings set prompt \"{PROMPT}\""))
            .await;
        session.send_command("settings set auto-confirm true").await;
        session
            .send_command("settings set stop-line-count-before 0")
            .await;
        session
            .send_command("settings set stop-line-count-after 0")
            .await;

        if !env_vars.is_empty() {
            let joined = env_vars
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            session
                .send_command(&format!("settings set target.env-vars {joined}"))
                .await;
        }

        for bp in breakpoints {
            session
                .send_command(&format!(
                    "breakpoint set --file \"{}\" --line {}",
                    bp.file, bp.line
                ))
                .await;
        }

        session.ready = true;
        let run_output = session.send_command("run").await;
        session.handle_stop_if_needed(&run_output).await;

        // Hand the ready session to its actor task.
        let (req_tx, req_rx) = mpsc::channel(16);
        let task = tokio::spawn(session.run(req_rx));
        self.session = Some(SessionHandle { req_tx, task });
        Ok(())
    }

    /// Stop the session: `kill` + `quit`, SIGKILL after 1s. Port of `stop`
    /// (`debug-driver.ts:100-112`). No-op if nothing is running.
    pub async fn stop(&mut self) {
        if let Some(handle) = self.session.take() {
            let (tx, rx) = oneshot::channel();
            // If the actor already exited (process closed), the send fails and
            // there is nothing left to stop.
            if handle.req_tx.send(Request::Stop(tx)).await.is_ok() {
                let _ = rx.await;
            }
            let _ = handle.task.await;
        }
    }

    /// Add a breakpoint mid-session. Port of `setBreakpoint`
    /// (`debug-driver.ts:114-117`).
    pub async fn set_breakpoint(&self, file: &str, line: u32) {
        self.request(|reply| Request::SetBreakpoint {
            file: file.to_string(),
            line,
            reply,
        })
        .await;
    }

    /// Remove a breakpoint mid-session. Port of `removeBreakpoint`
    /// (`debug-driver.ts:119-134`).
    pub async fn remove_breakpoint(&self, file: &str, line: u32) {
        self.request(|reply| Request::RemoveBreakpoint {
            file: file.to_string(),
            line,
            reply,
        })
        .await;
    }

    /// `continue` (`debug-driver.ts:136-140`).
    pub async fn continue_(&self) {
        self.request(Request::Continue).await;
    }

    /// `next` — step over (`debug-driver.ts:142-146`).
    pub async fn step_over(&self) {
        self.request(Request::StepOver).await;
    }

    /// `step` — step into (`debug-driver.ts:148-152`).
    pub async fn step_into(&self) {
        self.request(Request::StepInto).await;
    }

    /// `finish` — step out (`debug-driver.ts:154-158`).
    pub async fn step_out(&self) {
        self.request(Request::StepOut).await;
    }

    /// Fetch one level of members behind a pointer / expression path via
    /// `frame variable -T -P 1 -- <expr>`. Port of `getVarChildren`
    /// (`debug-driver.ts:379-386`).
    pub async fn get_var_children(&self, expr: &str) -> Vec<LocalVariable> {
        let Some(handle) = &self.session else {
            return Vec::new();
        };
        let (tx, rx) = oneshot::channel();
        if handle
            .req_tx
            .send(Request::GetVarChildren {
                expr: expr.to_string(),
                reply: tx,
            })
            .await
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Send a unit-reply request, waiting for the actor to finish it. Silently
    /// no-ops when there is no live session (mirrors the TS `if (!this.proc ||
    /// !this.ready) return;` guards on every control method).
    async fn request(&self, make: impl FnOnce(oneshot::Sender<()>) -> Request) {
        let Some(handle) = &self.session else {
            return;
        };
        let (tx, rx) = oneshot::channel();
        if handle.req_tx.send(make(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

/// All protocol state for a single lldb process, owned by exactly one actor
/// task after the handshake.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Accumulator for the in-flight command's output (or unsolicited output
    /// while idle). Cleared at each command boundary, exactly like TS `buffer`.
    buffer: String,
    /// Raw bytes of an incomplete trailing UTF-8 sequence, held back until the
    /// continuation bytes arrive (we read the pipe one byte at a time).
    pending: Vec<u8>,
    events: mpsc::UnboundedSender<DebugEvent>,
    ready: bool,
    exit_sent: bool,
}

impl Session {
    /// Idle loop: no command is pending, so stdout belongs to us. Handles
    /// control [`Request`]s and the 80ms async-settle window for unsolicited
    /// stop/exit output (port of `onData`'s idle branch + `handleAsyncOutput`,
    /// `debug-driver.ts:184-215`).
    async fn run(mut self, mut req_rx: mpsc::Receiver<Request>) {
        // When `Some`, unsolicited output is accumulating and we inspect it once
        // the stream stays quiet until this deadline (the TS `asyncTimer`).
        let mut async_deadline: Option<Instant> = None;

        loop {
            // Placeholder far-future deadline when the async window is inactive;
            // the branch's `if` precondition keeps it from ever firing.
            let sleep_deadline =
                async_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

            tokio::select! {
                biased;

                maybe_req = req_rx.recv() => {
                    let Some(req) = maybe_req else { break }; // driver dropped
                    // A command is about to run: cancel any pending async
                    // inspection (TS clears asyncTimer in sendCommand).
                    async_deadline = None;
                    if self.handle_request(req).await {
                        break; // Stop request
                    }
                }

                read = self.stdout.read_u8() => {
                    match read {
                        Ok(byte) => {
                            self.push_byte(byte);
                            // (Re)arm the 80ms quiet window for unsolicited output.
                            async_deadline = Some(Instant::now() + ASYNC_SETTLE);
                        }
                        Err(_) => {
                            // EOF / read error: the process closed its stdout.
                            self.on_process_closed().await;
                            break;
                        }
                    }
                }

                _ = sleep_until(sleep_deadline), if async_deadline.is_some() => {
                    async_deadline = None;
                    self.handle_async_output().await;
                }
            }
        }
    }

    /// Dispatch one control request. Returns `true` for `Stop` (loop should
    /// end).
    async fn handle_request(&mut self, req: Request) -> bool {
        match req {
            Request::SetBreakpoint { file, line, reply } => {
                if self.ready {
                    self.send_command(&format!(
                        "breakpoint set --file \"{file}\" --line {line}"
                    ))
                    .await;
                }
                let _ = reply.send(());
            }
            Request::RemoveBreakpoint { file, line, reply } => {
                if self.ready {
                    self.remove_breakpoint(&file, line).await;
                }
                let _ = reply.send(());
            }
            Request::Continue(reply) => {
                self.step_command("continue").await;
                let _ = reply.send(());
            }
            Request::StepOver(reply) => {
                self.step_command("next").await;
                let _ = reply.send(());
            }
            Request::StepInto(reply) => {
                self.step_command("step").await;
                let _ = reply.send(());
            }
            Request::StepOut(reply) => {
                self.step_command("finish").await;
                let _ = reply.send(());
            }
            Request::GetVarChildren { expr, reply } => {
                let children = if self.ready {
                    self.get_var_children(&expr).await
                } else {
                    Vec::new()
                };
                let _ = reply.send(children);
            }
            Request::Stop(reply) => {
                self.do_stop().await;
                let _ = reply.send(());
                return true;
            }
        }
        false
    }

    /// A stepping command (`continue`/`next`/`step`/`finish`) followed by
    /// stop-detection (`debug-driver.ts:136-158`).
    async fn step_command(&mut self, cmd: &str) {
        if !self.ready {
            return;
        }
        let output = self.send_command(cmd).await;
        self.handle_stop_if_needed(&output).await;
    }

    /// `kill` + `quit`, then SIGKILL after 1s (`debug-driver.ts:100-112`).
    async fn do_stop(&mut self) {
        let _ = self.stdin.write_all(b"kill\n").await;
        let _ = self.stdin.write_all(b"quit\n").await;
        let _ = self.stdin.flush().await;
        match tokio::time::timeout(KILL_FALLBACK, self.child.wait()).await {
            Ok(Ok(status)) => self.emit_exit(status.code().unwrap_or(-1)),
            _ => {
                let _ = self.child.start_kill();
                let _ = self.child.wait().await;
                self.emit_exit(-1);
            }
        }
    }

    /// stdout EOF: the process is gone. Emit `Exited` once
    /// (`debug-driver.ts:53-60`).
    async fn on_process_closed(&mut self) {
        let code = match self.child.wait().await {
            Ok(status) => status.code().unwrap_or(-1),
            Err(_) => -1,
        };
        self.emit_exit(code);
    }

    fn emit_exit(&mut self, code: i32) {
        if !self.exit_sent {
            self.exit_sent = true;
            let _ = self.events.send(DebugEvent::Exited(code));
        }
    }

    /// Append one stdout byte to the command buffer and forward it as `Output`.
    /// Bytes for an incomplete UTF-8 sequence are held in `pending` until the
    /// continuation arrives, so we never emit or buffer a partial code point.
    /// The `Output` passthrough mirrors `onData` emitting every chunk verbatim.
    fn push_byte(&mut self, byte: u8) {
        let before = self.buffer.len();
        self.pending.push(byte);
        if let Ok(s) = std::str::from_utf8(&self.pending) {
            self.buffer.push_str(s);
            let added = &self.buffer[before..];
            if !added.is_empty() {
                let _ = self.events.send(DebugEvent::Output(added.to_string()));
            }
            self.pending.clear();
        }
    }

    /// Wait for lldb's initial banner to fully drain, so the first command
    /// starts cleanly aligned. Port of `waitForPrompt`
    /// (`debug-driver.ts:217-232`).
    ///
    /// This lldb block-buffers its trailing `(lldb) ` prompt when stdout is a
    /// pipe — the prompt is not flushed until we send the first command — so we
    /// cannot rely on seeing it. We resolve on the line-start prompt when it
    /// *does* flush, otherwise on a quiet-settle once the banner stops flowing
    /// (the robust analogue of the TS 100ms settle poke), bounded by
    /// `PROMPT_WAIT`.
    async fn wait_for_prompt(&mut self) {
        let overall = Instant::now() + PROMPT_WAIT;
        loop {
            if self.buffer.ends_with(LLDB_PROMPT_LINE) || self.buffer.ends_with(PROMPT_LINE) {
                break;
            }
            let settle = Instant::now() + SETTLE;
            tokio::select! {
                biased;
                read = self.stdout.read_u8() => match read {
                    Ok(byte) => self.push_byte(byte),
                    Err(_) => break,
                },
                // Banner has stopped flowing (some already received) — proceed
                // even though the prompt itself is still buffered in lldb.
                _ = sleep_until(settle), if !self.buffer.is_empty() => break,
                _ = sleep_until(overall) => break,
            }
        }
        self.buffer.clear();
        self.pending.clear();
    }

    /// Write a command and collect its output, resolving on a trailing prompt,
    /// a 150ms quiet-settle, or the 10s command timeout — whichever comes first.
    /// Port of `sendCommand` + `onData`'s pending branch
    /// (`debug-driver.ts:168-182, 236-264`).
    async fn send_command(&mut self, cmd: &str) -> String {
        if self.stdin.write_all(cmd.as_bytes()).await.is_err()
            || self.stdin.write_all(b"\n").await.is_err()
        {
            return String::new();
        }
        let _ = self.stdin.flush().await;

        self.buffer.clear();
        self.pending.clear();
        let overall = Instant::now() + COMMAND_TIMEOUT;

        loop {
            // Fast path: a line-start trailing prompt definitely ends the
            // response (see PROMPT_LINE — must not match echoed/leading prompts).
            if self.buffer.ends_with(PROMPT_LINE) {
                break;
            }
            let settle = Instant::now() + SETTLE;
            tokio::select! {
                biased;
                read = self.stdout.read_u8() => match read {
                    Ok(byte) => self.push_byte(byte),
                    Err(_) => break, // EOF: return buffer-so-far
                },
                _ = sleep_until(settle) => break,   // 150ms quiet-settle
                _ = sleep_until(overall) => break,  // 10s command timeout
            }
        }

        let output = std::mem::take(&mut self.buffer);
        self.pending.clear();
        output
    }

    /// Inspect unsolicited output for stop / exit events (the 80ms window fired)
    /// — port of `handleAsyncOutput` (`debug-driver.ts:208-215`).
    async fn handle_async_output(&mut self) {
        let output = self.buffer.clone();
        if STOP_RE.is_match(&output) || output.contains("exited with status") {
            self.buffer.clear();
            self.pending.clear();
            self.handle_stop_if_needed(&output).await;
        }
    }

    /// React to a stop / exit found in `output`. Port of `handleStopIfNeeded`
    /// (`debug-driver.ts:266-312`): emits `Exited` on process exit, otherwise
    /// parses the stop reason + location, runs `frame variable -T` then `bt`
    /// sequentially, and emits `Stopped`.
    async fn handle_stop_if_needed(&mut self, output: &str) {
        if let Some(m) = EXIT_RE.captures(output) {
            let code: i32 = m[1].parse().unwrap_or(-1);
            self.emit_exit(code);
            let _ = self.stdin.write_all(b"quit\n").await;
            let _ = self.stdin.flush().await;
            return;
        }

        let Some(stop) = STOP_RE.captures(output) else {
            return;
        };
        let reason = stop[1].trim().to_string();

        // Location from "at file:line"; fall back to frame #0 below.
        let mut file = String::new();
        let mut line: u32 = 0;
        if let Some(loc) = LOC_RE.captures(output) {
            file = loc[1].to_string();
            line = loc[2].parse().unwrap_or(0);
        }

        // Sequential — lldb answers one command at a time. `-T` annotates types
        // at every depth so pointers stay expandable.
        let locals_output = self.send_command("frame variable -T").await;
        let bt_output = self.send_command("bt").await;

        let locals = parse_variables(&locals_output, None);
        let frames = parse_backtrace(&bt_output);

        if file.is_empty() {
            if let Some(f0) = frames.first() {
                file = f0.file.clone();
                line = f0.line;
            }
        }

        let _ = self.events.send(DebugEvent::Stopped {
            reason,
            file,
            line,
            frames,
            locals,
        });
    }

    /// List breakpoints, find the one matching basename + line, delete it. Port
    /// of `removeBreakpoint` (`debug-driver.ts:119-134`).
    async fn remove_breakpoint(&mut self, file: &str, line: u32) {
        let output = self.send_command("breakpoint list").await;
        let basename = std::path::Path::new(file)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(file);
        let line_marker = format!("line = {line}");
        for l in output.split('\n') {
            // Match id lines: "N: file = '...', line = N, ..."
            let Some(id) = l.split(':').next().filter(|s| {
                !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
            }) else {
                continue;
            };
            if l.contains(basename) && l.contains(&line_marker) {
                let id = id.to_string();
                self.send_command(&format!("breakpoint delete {id}")).await;
                return;
            }
        }
    }

    /// Fetch one level of members behind a pointer / expression path. Port of
    /// `getVarChildren` (`debug-driver.ts:379-386`).
    async fn get_var_children(&mut self, expr: &str) -> Vec<LocalVariable> {
        let output = self
            .send_command(&format!("frame variable -T -P 1 -- {expr}"))
            .await;
        let mut roots = parse_variables(&output, Some(expr));
        if roots.is_empty() {
            return Vec::new();
        }
        roots.swap_remove(0).children.unwrap_or_default()
    }
}
