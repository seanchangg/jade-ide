//! Data types shared across the debug driver, ported from the TypeScript
//! interfaces in `src/shared/types.ts` (`StackFrame`, `LocalVariable`,
//! `DebugStoppedEvent`) plus the driver's `Breakpoint` input.
//!
//! Field names serialize to the same camelCase JSON the Electron renderer
//! expects (`type`, `functionName`), so Phase-3 can forward these events over
//! IPC without a translation layer.

use serde::{Deserialize, Serialize};

/// A source breakpoint request. Port of `Breakpoint` (`types.ts`) — only the
/// fields the driver consumes (`file`, `line`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Breakpoint {
    pub file: String,
    pub line: u32,
}

impl Breakpoint {
    pub fn new(file: impl Into<String>, line: u32) -> Self {
        Breakpoint {
            file: file.into(),
            line,
        }
    }
}

/// One frame of a backtrace. Port of `StackFrame` (`types.ts:247-252`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackFrame {
    pub index: u32,
    #[serde(rename = "functionName")]
    pub function_name: String,
    pub file: String,
    pub line: u32,
}

/// A local variable / expandable node in the variables tree. Port of
/// `LocalVariable` (`types.ts:254-264`). `path` is the lldb expression path
/// (`m.inner.q`, `vec[0]`, `*m.buf`) used for lazy child expansion;
/// `expandable` is set when children exist or can be fetched (non-null
/// pointers). `None` fields are omitted from JSON, matching the optional TS
/// fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalVariable {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<LocalVariable>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expandable: Option<bool>,
}

/// Events streamed from the driver to the UI layer over a tokio mpsc channel.
/// The Rust analogue of the renderer IPC channels `debug:output` /
/// `debug:stopped` / `debug:exited`. `Stopped` bundles what
/// `DebugStoppedEvent` (`types.ts:268-274`) carried.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DebugEvent {
    /// Raw stdout/stderr passthrough (`IPC.DEBUG_OUTPUT`).
    Output(String),
    /// Process stopped at a breakpoint / after a step (`IPC.DEBUG_STOPPED`).
    Stopped {
        reason: String,
        file: String,
        line: u32,
        frames: Vec<StackFrame>,
        locals: Vec<LocalVariable>,
    },
    /// Process exited (`IPC.DEBUG_EXITED`). Sent at most once per session.
    Exited(i32),
}
