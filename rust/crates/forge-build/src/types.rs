//! Request / result / event types for the headless build engine.
//!
//! These mirror the payloads `src/main/build-runner.ts` exchanged over IPC
//! (`BuildResult`, `AllocationEvent`, the run result object, the ASM result).
//! They derive `Serialize`/`Deserialize` so a Phase-3 IPC/UI layer can forward
//! them unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Diagnostic severity, as emitted by the compiler / CMake error regexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Note,
}

/// A parsed compiler or CMake diagnostic (build-runner.ts `BuildResult['errors']`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildError {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub message: String,
    pub severity: Severity,
}

/// Input to [`crate::BuildEngine::compile`] (build-runner.ts `compile()` args).
#[derive(Debug, Clone, Default)]
pub struct CompileRequest {
    /// The active source file whose project is being built.
    pub file: PathBuf,
    /// Extra user CXX flags (appended after the base `-g`).
    pub flags: Vec<String>,
    /// Add `-fsanitize=address -fno-omit-frame-pointer` (+ linker flag).
    pub sanitize: bool,
    /// Add `-fprofile-arcs -ftest-coverage` (+ `--coverage` linker flag).
    pub instrument: bool,
}

/// Result of a compile (build-runner.ts `BuildResult`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
    pub errors: Vec<BuildError>,
    pub duration: Duration,
}

/// Input to [`crate::BuildEngine::run`] (build-runner.ts `run()` config).
#[derive(Debug, Clone, Default)]
pub struct RunConfig {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub enable_sanitizers: bool,
    pub enable_instrumentation: bool,
}

/// Whether a memory event is an allocation or a free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllocKind {
    Alloc,
    Free,
}

/// A single `__FORGE_ALLOC|` / `__FORGE_FREE|` line (build-runner.ts `AllocationEvent`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocEvent {
    #[serde(rename = "type")]
    pub kind: AllocKind,
    pub pointer: String,
    pub size: i64,
    pub file: String,
    pub line: u32,
    pub timestamp: f64,
}

/// Memory-subsystem events surfaced to the UI. Allocation/free lines are
/// **batched** (perf-findings.md #1); every other variant flushes immediately.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MemoryEvent {
    /// A coalesced batch of alloc/free events (the firehose fix).
    #[serde(rename = "alloc-batch")]
    AllocBatch { events: Vec<AllocEvent> },
    /// `__FORGE_HEAP_SUMMARY|` from the malloc interposer.
    #[serde(rename = "heap-summary")]
    HeapSummary {
        total_alloc: i64,
        total_freed: i64,
        current_heap: i64,
        peak_heap: i64,
        alloc_count: i64,
        free_count: i64,
    },
    /// ASan `SUMMARY: ... N byte(s) leaked in M allocation(s)`.
    #[serde(rename = "asan-leak-summary")]
    AsanLeakSummary {
        leaked_bytes: i64,
        leaked_allocations: i64,
    },
    /// A per-leak `#N 0x.. in func file:line` frame.
    #[serde(rename = "asan-leak-location")]
    AsanLeakLocation {
        function_name: String,
        file: String,
        line: u32,
    },
    /// ASan allocation stats (either the `Stats:` line or the `print_stats=1` block).
    #[serde(rename = "asan-stats")]
    AsanStats {
        total_allocations: i64,
        total_frees: i64,
        total_bytes: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_freed_bytes: Option<i64>,
    },
}

/// Events emitted on the [`RunHandle`] channel while a program runs.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// A line of ordinary program output (already newline-terminated), or an
    /// engine notice (e.g. the red `[ASan] <type>` markers).
    Output(String),
    /// A memory/telemetry-memory event.
    Memory(MemoryEvent),
}

/// `-finstrument-functions` roll-up (build-runner.ts run-result
/// `instrumentationSummary`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentationSummary {
    pub unique_functions: usize,
    pub max_call_depth: u32,
}

/// Terminal result of a run (build-runner.ts run-result object).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub exit_code: i32,
    pub duration: Duration,
    /// line number → execution count (from `__FORGE_TRACE` and/or gcov).
    pub executed_lines: HashMap<u32, u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sanitizer_output: Option<String>,
    pub interpose_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instrumentation_summary: Option<InstrumentationSummary>,
}

/// Result of [`crate::BuildEngine::generate_asm`] (build-runner.ts
/// `generateAssembly` return).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsmResult {
    pub success: bool,
    pub asm: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// filtered-asm line (1-based) → source line.
    pub asm_to_source: HashMap<usize, u32>,
}
