//! `forge-lsp` — the clangd LSP client for Jade.
//!
//! A **direct, thin JSON-RPC client** over clangd's stdio: it owns its base
//! protocol framing ([`framing`]) and speaks LSP with the [`lsp_types`] types,
//! depending on no editor/LSP transport crates. This is the recorded
//! architectural decision — *not* zed's `project`/`LspStore`.
//!
//! It is a faithful port of the Electron `LspClient` in
//! `src/main/lsp-client.ts` (spawn flags, initialize capabilities,
//! `initializationOptions.fallbackFlags`, language-id mapping, request set, the
//! shutdown handshake, and the custom `memory-info`/`sizeof` feature), with one
//! deliberate improvement: **incremental `didChange`** negotiated against the
//! server's [`lsp_types::TextDocumentSyncKind`] (see [`DidChange`]).
//!
//! # Usage sketch
//!
//! ```no_run
//! # use forge_lsp::{LspClient, LspEvent};
//! # use std::path::Path;
//! # async fn go() -> Result<(), Box<dyn std::error::Error>> {
//! let mut handle = LspClient::initialize(Path::new("/proj"), Some(Path::new("/proj/include"))).await?;
//! let mut events = handle.take_events().unwrap();
//! tokio::spawn(async move {
//!     while let Some(ev) = events.recv().await {
//!         if let LspEvent::Diagnostics { path, diagnostics } = ev {
//!             // forward to the editor
//!             let _ = (path, diagnostics);
//!         }
//!     }
//! });
//! handle.did_open(Path::new("/proj/main.cpp"), "int main(){}", 1)?;
//! # Ok(()) }
//! ```
//!
//! ## Threading & cancellation
//!
//! All I/O runs on two detached tokio tasks (reader + actor); [`LspHandle`]
//! methods only pass messages, so the handle is usable from any task and never
//! blocks the caller beyond [`REQUEST_TIMEOUT`]. Superseding an in-flight
//! completion is the caller's job — drop the future and issue a new
//! [`LspHandle::completion`]; we send no `$/cancelRequest`.

mod client;
pub mod framing;
mod uri;

pub use client::{
    DidChange, LspClient, LspError, LspEvent, LspHandle, Result, Utf16RangeEdit, REQUEST_TIMEOUT,
};

/// The file-path → clangd `languageId` mapping (`.h/.hpp/.hxx → cpp`, `.c → c`,
/// `.m → objective-c`, `.mm → objective-cpp`, `.cu → cuda`, else `cpp`). Ported
/// from `src/main/lsp-client.ts:234-241`.
pub use uri::language_id;

// Re-export the LSP domain types callers need, so jade depends only on
// `forge-lsp` for the request/response vocabulary.
pub use lsp_types::{
    CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity, Hover, HoverContents,
    Location, MarkupContent, Position, Range, TextDocumentSyncKind,
};
