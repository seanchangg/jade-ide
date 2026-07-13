//! Jade LLDB debug driver — Rust port of `src/main/debug-driver.ts`.
//!
//! Wraps an interactive `lldb <executable>` process: a custom prompt, suppressed
//! stop context, breakpoint/stepping control, and a stateful parser turning
//! `frame variable` / `bt` text into a variables tree + backtrace. Semantics
//! preserved from the TS driver (see `docs/jade-feature-inventory.md` §8.4):
//!
//! - output framing: trailing-prompt match OR 150ms quiet-settle ends a command
//!   reply; unsolicited output waits 80ms then is inspected for a stop/exit
//! - one in-flight command (serialized through the session actor); 10s per-
//!   command timeout resolving with the buffer-so-far
//! - `start` is idempotent (stops a prior session); `stop` = `kill`+`quit` then
//!   SIGKILL after 1s
//! - on stop: `frame variable -T` then `bt`, sequentially; location from
//!   `at file:line` with first-backtrace-frame fallback
//! - env injection seam (`env_vars` on `start`): Phase-3 supplies
//!   `FORGE_TELEMETRY_SOCK` + the probe dylib, applied via `target.env-vars`
//!   (no malloc interposer while debugging)
//!
//! Not ported: the Electron IPC glue (`registerDebugHandlers`,
//! `debug-driver.ts:408-432`). Phase-3 owns wiring [`DebugEvent`]s and the
//! [`LldbDriver`] methods to the renderer.

mod driver;
mod parse;
mod types;

pub use driver::LldbDriver;
pub use types::{Breakpoint, DebugEvent, LocalVariable, StackFrame};

// Re-exported for direct testing / reuse by higher layers.
pub use parse::{parse_backtrace, parse_variables};
