//! `run()` — launch the built program with telemetry/dylib injection, stream
//! and classify its stdout/stderr, and collect a [`RunResult`]
//! (build-runner.ts:561-793).
//!
//! Firehose fix (perf-findings.md #1): `__JADE_ALLOC`/`__JADE_FREE` lines are
//! accumulated and emitted as [`MemoryEvent::AllocBatch`] batches — flushed
//! every 75 ms or every 500 events — instead of one channel send per line.
//! Every other event type flushes immediately, matching the TS.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::dylib::{ensure_interpose_dylib, ensure_probe_dylib, INTERPOSE_DYLIB, PROBE_DYLIB};
use crate::parse::*;
use crate::symbolicate::AtosSymbolicator;
use crate::types::*;
use crate::{BuildEngine, EngineConfig};

/// Batch flush cadence (perf-findings.md #1 recommends 50–100 ms / 500 events).
const BATCH_FLUSH: Duration = Duration::from_millis(75);
const BATCH_CAP: usize = 500;

/// Handle to a running program: an event stream plus the eventual result.
pub struct RunHandle {
    /// Program output and memory events, in arrival order.
    pub events: mpsc::UnboundedReceiver<RunEvent>,
    /// Resolves when the program exits (or is killed) with its [`RunResult`].
    pub result: tokio::task::JoinHandle<RunResult>,
}

/// Spawn a run, killing any previous one (single concurrent run, build-runner.ts:565-568).
pub fn spawn_run(engine: &BuildEngine, cfg: RunConfig) -> RunHandle {
    // Kill the previous run by signalling its loop.
    if let Some(prev) = engine.run_stop.lock().unwrap().take() {
        let _ = prev.send(());
    }
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    *engine.run_stop.lock().unwrap() = Some(stop_tx);

    // Install the atos symbolicator carrying this run's executable, so buffer
    // aliasing on the telemetry side can find sources even without meta.exe.
    if let Some(tel) = &engine.config.telemetry {
        tel.set_symbolicator(Arc::new(AtosSymbolicator::new(cfg.executable.clone())));
    }

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let config = engine.config.clone();
    let result = tokio::spawn(async move { run_task(cfg, config, events_tx, stop_rx).await });
    RunHandle {
        events: events_rx,
        result,
    }
}

fn error_result(start: Instant, msg: String) -> RunResult {
    RunResult {
        exit_code: -1,
        duration: start.elapsed(),
        executed_lines: HashMap::new(),
        sanitizer_output: Some(msg),
        interpose_active: false,
        instrumentation_summary: None,
    }
}

async fn run_task(
    cfg: RunConfig,
    config: EngineConfig,
    events_tx: mpsc::UnboundedSender<RunEvent>,
    mut stop_rx: oneshot::Receiver<()>,
) -> RunResult {
    let start = Instant::now();
    // Run from the project root when the caller provides it (matches CLion, so
    // relative paths like `fopen("drake_lyrics.txt")` resolve); otherwise fall
    // back to the executable's parent (the build dir).
    let cwd = cfg
        .cwd
        .clone()
        .or_else(|| cfg.executable.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    // ── Environment & dylib injection ──
    let mut cmd = tokio::process::Command::new(&cfg.executable);
    cmd.args(&cfg.args)
        .current_dir(&cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Some(tel) = &config.telemetry {
        cmd.env("JADE_TELEMETRY_SOCK", tel.socket_path());
    }
    if cfg.enable_sanitizers {
        cmd.env("ASAN_OPTIONS", "detect_leaks=0:print_stats=1");
    }
    // Malloc interposer conflicts with ASan → only when not sanitizing; the
    // Metal probe is safe with ASan → always attempted (build-runner.ts:592-608).
    let mut dylibs: Vec<&str> = Vec::new();
    let mut interpose_active = false;
    if !cfg.enable_sanitizers && ensure_interpose_dylib(&config.interpose_c) {
        dylibs.push(INTERPOSE_DYLIB);
        interpose_active = true;
    }
    if ensure_probe_dylib(&config.probe_mm) {
        dylibs.push(PROBE_DYLIB);
    }
    if !dylibs.is_empty() {
        cmd.env("DYLD_INSERT_LIBRARIES", dylibs.join(":"));
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return error_result(start, format!("Failed to run: {e}")),
    };

    let mut so = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut se = BufReader::new(child.stderr.take().expect("piped")).lines();

    // Collected state.
    let mut executed_lines: HashMap<u32, u32> = HashMap::new();
    let mut func_addresses: HashSet<String> = HashSet::new();
    let mut max_call_depth: u32 = 0;
    let mut cur_call_depth: u32 = 0;
    let mut sanitizer_output = String::new();
    let mut alloc_batch: Vec<AllocEvent> = Vec::new();

    let flush_batch = |batch: &mut Vec<AllocEvent>, tx: &mpsc::UnboundedSender<RunEvent>| {
        if !batch.is_empty() {
            let events = std::mem::take(batch);
            let _ = tx.send(RunEvent::Memory(MemoryEvent::AllocBatch { events }));
        }
    };

    let (mut so_done, mut se_done) = (false, false);
    let mut flush_timer = tokio::time::interval(BATCH_FLUSH);
    flush_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    flush_timer.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = &mut stop_rx => {
                let _ = child.start_kill();
                break;
            }
            line = so.next_line(), if !so_done => match line {
                Ok(Some(l)) => handle_stdout(&l, &mut alloc_batch, &events_tx, config.telemetry.as_ref()),
                _ => so_done = true,
            },
            line = se.next_line(), if !se_done => match line {
                Ok(Some(l)) => handle_stderr(
                    &l,
                    &mut executed_lines,
                    &mut func_addresses,
                    &mut cur_call_depth,
                    &mut max_call_depth,
                    &mut sanitizer_output,
                    &events_tx,
                ),
                _ => se_done = true,
            },
            _ = flush_timer.tick() => flush_batch(&mut alloc_batch, &events_tx),
        }
        if so_done && se_done {
            break;
        }
    }

    // Flush anything the interval didn't (partial-line already handled by lines()).
    flush_batch(&mut alloc_batch, &events_tx);

    let exit_code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);

    // ── Post-exit ASan parsing ──
    if cfg.enable_sanitizers && !sanitizer_output.is_empty() {
        let (events, outputs) = parse_asan_output(&sanitizer_output);
        for e in events {
            let _ = events_tx.send(RunEvent::Memory(e));
        }
        for o in outputs {
            let _ = events_tx.send(RunEvent::Output(o));
        }
        for e in parse_asan_stats(&sanitizer_output) {
            let _ = events_tx.send(RunEvent::Memory(e));
        }
    }

    // ── Coverage via gcov (instrument mode) ──
    if cfg.enable_instrumentation {
        process_gcov(&cwd, &mut executed_lines, &events_tx);
    }

    // Always clean up stray .gcov report files gcov writes into cwd.
    if let Ok(entries) = std::fs::read_dir(&cwd) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with(".gcov") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    let instrumentation_summary = if !func_addresses.is_empty() {
        Some(InstrumentationSummary {
            unique_functions: func_addresses.len(),
            max_call_depth,
        })
    } else {
        None
    };

    RunResult {
        exit_code,
        duration: start.elapsed(),
        executed_lines,
        sanitizer_output: if sanitizer_output.is_empty() {
            None
        } else {
            Some(sanitizer_output)
        },
        interpose_active,
        instrumentation_summary,
    }
}

/// Classify one stdout line (build-runner.ts `parseJadeOutput` + the stdout
/// loop). Alloc/free batch; scalar/timing route through telemetry; other
/// `__JADE_*` lines are swallowed; everything else is program output.
///
/// Deviation: the TS printed *unrecognized* `__JADE_*` stdout lines as program
/// output (its `parseJadeOutput` returned `false`); we swallow them per the
/// inventory §8.2 ("Unrecognized `__JADE_*` lines are swallowed, not printed")
/// and the explicit task spec.
fn handle_stdout(
    line: &str,
    batch: &mut Vec<AllocEvent>,
    tx: &mpsc::UnboundedSender<RunEvent>,
    telemetry: Option<&Arc<jade_telemetry::TelemetryServer>>,
) {
    let t = line.trim();
    if let Some(ev) = parse_alloc_free(t) {
        batch.push(ev);
        if batch.len() >= BATCH_CAP {
            let events = std::mem::take(batch);
            let _ = tx.send(RunEvent::Memory(MemoryEvent::AllocBatch { events }));
        }
    } else if let Some(s) = parse_scalar(t) {
        if let Some(tel) = telemetry {
            tel.ingest_scalar(s);
        }
    } else if let Some(tm) = parse_timing(t) {
        if let Some(tel) = telemetry {
            tel.ingest_timing(tm);
        }
    } else if t.starts_with("__JADE_") {
        // Unrecognized instrumentation line — swallow (see doc comment).
    } else {
        let _ = tx.send(RunEvent::Output(format!("{line}\n")));
    }
}

/// Classify one stderr line (build-runner.ts:641-677). Tracks coverage/trace
/// counters, emits heap summaries immediately, accumulates the raw text for the
/// post-exit ASan parse. Same swallow-unrecognized-`__JADE_` deviation as
/// [`handle_stdout`].
#[allow(clippy::too_many_arguments)]
fn handle_stderr(
    line: &str,
    executed_lines: &mut HashMap<u32, u32>,
    func_addresses: &mut HashSet<String>,
    cur_call_depth: &mut u32,
    max_call_depth: &mut u32,
    sanitizer_output: &mut String,
    tx: &mpsc::UnboundedSender<RunEvent>,
) {
    sanitizer_output.push_str(line);
    sanitizer_output.push('\n');

    if line.starts_with("__JADE_TRACE|") {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 3 {
            if let Ok(n) = parts[2].parse::<u32>() {
                *executed_lines.entry(n).or_insert(0) += 1;
            }
        }
    } else if line.starts_with("__JADE_FUNC_ENTER|") {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 3 {
            func_addresses.insert(parts[1].to_string());
            *cur_call_depth += 1;
            if *cur_call_depth > *max_call_depth {
                *max_call_depth = *cur_call_depth;
            }
        }
    } else if line.starts_with("__JADE_FUNC_EXIT|") {
        *cur_call_depth = cur_call_depth.saturating_sub(1);
    } else if line.starts_with("__JADE_HEAP_SUMMARY|") {
        if let Some(ev) = parse_heap_summary(line) {
            let _ = tx.send(RunEvent::Memory(ev));
        }
    } else if line.starts_with("__JADE_INTERPOSE_ACTIVE") {
        // Interposer active marker — nothing to forward.
    } else if line.starts_with("__JADE_") {
        // Unrecognized instrumentation line — swallow (see doc comment).
    } else if !line.is_empty() {
        let _ = tx.send(RunEvent::Output(format!("{line}\n")));
    }
}

/// Recursively collect files ending in `ext` up to `max_depth`
/// (build-runner.ts `findFilesRecursive`, default depth 6).
fn find_files_recursive(dir: &Path, ext: &str, max_depth: i32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if max_depth < 0 {
        return out;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(find_files_recursive(&path, ext, max_depth - 1));
        } else if entry.file_name().to_string_lossy().ends_with(ext) {
            out.push(path);
        }
    }
    out
}

/// gcov coverage extraction (build-runner.ts:709-759). Preserves the quirk of
/// running gcov on the **first** `.gcda` only; cleans up `.gcov` and processed
/// `.gcda` (keeps `.gcno`); emits a cyan coverage summary.
fn process_gcov(
    cwd: &Path,
    executed_lines: &mut HashMap<u32, u32>,
    tx: &mpsc::UnboundedSender<RunEvent>,
) {
    let gcda_files = find_files_recursive(cwd, ".gcda", 6);
    if let Some(first) = gcda_files.first() {
        let parent = first.parent().unwrap_or(cwd);
        let mut cmd = std::process::Command::new("gcov");
        cmd.arg("-o").arg(parent).arg(first).current_dir(cwd);
        let _ = crate::util::output_with_timeout(cmd, Duration::from_secs(10));

        // Parse `.cpp.gcov` outputs in cwd, set absolute counts, delete them.
        if let Ok(entries) = std::fs::read_dir(cwd) {
            let gcov_files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().ends_with(".cpp.gcov"))
                .collect();
            for gf in gcov_files {
                if let Ok(content) = std::fs::read_to_string(&gf) {
                    for (line_no, count) in parse_gcov(&content) {
                        executed_lines.insert(line_no, count);
                    }
                }
                let _ = std::fs::remove_file(&gf);
            }
        }
        // Clean up any other generated .gcov files.
        if let Ok(entries) = std::fs::read_dir(cwd) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().ends_with(".gcov") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    // Clean up processed .gcda files (keep .gcno — the build owns those).
    for gf in &gcda_files {
        let _ = std::fs::remove_file(gf);
    }

    if !executed_lines.is_empty() {
        let total: u64 = executed_lines.values().map(|&v| v as u64).sum();
        let _ = tx.send(RunEvent::Output(format!(
            "\x1b[36m[jade]\x1b[0m Coverage: {} lines executed ({} total hits)\n",
            executed_lines.len(),
            total
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut mpsc::UnboundedReceiver<RunEvent>) -> Vec<RunEvent> {
        let mut v = Vec::new();
        while let Ok(e) = rx.try_recv() {
            v.push(e);
        }
        v
    }

    #[test]
    fn stdout_batches_alloc_free_and_swallows_unknown_jade() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut batch = Vec::new();
        handle_stdout("__JADE_ALLOC|0x1|8|a.cpp|1|0.0", &mut batch, &tx, None);
        handle_stdout("__JADE_FREE|0x1|8|a.cpp|1|1.0", &mut batch, &tx, None);
        handle_stdout("hello world", &mut batch, &tx, None);
        handle_stdout("__JADE_MYSTERY|whatever", &mut batch, &tx, None);
        // alloc/free are buffered, not yet sent; "hello world" is output; the
        // unknown __JADE_ line is swallowed.
        assert_eq!(batch.len(), 2);
        let evs = drain(&mut rx);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0], RunEvent::Output("hello world\n".into()));
    }

    #[test]
    fn stdout_flushes_batch_at_cap() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut batch = Vec::new();
        for i in 0..BATCH_CAP {
            handle_stdout(
                &format!("__JADE_ALLOC|0x{i}|8|a.cpp|1|0.0"),
                &mut batch,
                &tx,
                None,
            );
        }
        assert!(batch.is_empty(), "batch flushed at cap");
        let evs = drain(&mut rx);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            RunEvent::Memory(MemoryEvent::AllocBatch { events }) => {
                assert_eq!(events.len(), BATCH_CAP);
            }
            _ => panic!("expected AllocBatch"),
        }
    }

    #[test]
    fn stderr_tracks_trace_and_instrumentation() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut executed = HashMap::new();
        let mut funcs = HashSet::new();
        let (mut cur, mut max) = (0u32, 0u32);
        let mut san = String::new();

        let call = |l: &str,
                    executed: &mut HashMap<u32, u32>,
                    funcs: &mut HashSet<String>,
                    cur: &mut u32,
                    max: &mut u32,
                    san: &mut String,
                    tx: &mpsc::UnboundedSender<RunEvent>| {
            handle_stderr(l, executed, funcs, cur, max, san, tx);
        };

        call("__JADE_TRACE|f|42|x", &mut executed, &mut funcs, &mut cur, &mut max, &mut san, &tx);
        call("__JADE_TRACE|f|42|x", &mut executed, &mut funcs, &mut cur, &mut max, &mut san, &tx);
        call("__JADE_FUNC_ENTER|0xA|f", &mut executed, &mut funcs, &mut cur, &mut max, &mut san, &tx);
        call("__JADE_FUNC_ENTER|0xB|g", &mut executed, &mut funcs, &mut cur, &mut max, &mut san, &tx);
        call("__JADE_FUNC_EXIT|0xB", &mut executed, &mut funcs, &mut cur, &mut max, &mut san, &tx);
        call("__JADE_INTERPOSE_ACTIVE", &mut executed, &mut funcs, &mut cur, &mut max, &mut san, &tx);
        call("__JADE_HEAP_SUMMARY|1|2|3|4|5|6", &mut executed, &mut funcs, &mut cur, &mut max, &mut san, &tx);
        call("plain stderr", &mut executed, &mut funcs, &mut cur, &mut max, &mut san, &tx);

        assert_eq!(executed.get(&42), Some(&2)); // trace increments
        assert_eq!(funcs.len(), 2);
        assert_eq!(max, 2); // deepest nesting
        assert_eq!(cur, 1); // one exit

        let evs = drain(&mut rx);
        // heap summary (Memory) + plain stderr (Output); markers swallowed.
        assert!(evs.iter().any(|e| matches!(e, RunEvent::Memory(MemoryEvent::HeapSummary { .. }))));
        assert!(evs.iter().any(|e| *e == RunEvent::Output("plain stderr\n".into())));
        assert_eq!(evs.len(), 2);
    }
}
