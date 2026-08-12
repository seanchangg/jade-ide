//! `jade-lsp` — the clangd LSP client for Jade.
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
//! # use jade_lsp::{LspClient, LspEvent};
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
// `jade-lsp` for the request/response vocabulary.
pub use lsp_types::{
    CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity, Hover, HoverContents,
    Location, MarkupContent, ParameterInformation, ParameterLabel, Position, Range, SignatureHelp,
    SignatureInformation, TextDocumentSyncKind,
};

/// A flattened, render-ready signature hint: the active signature's full label
/// (e.g. `"Point(int x, int y)"`) and the byte range within that label of the
/// active parameter, so the editor can emphasize the argument being filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureHint {
    pub label: String,
    pub active_param: Option<std::ops::Range<usize>>,
}

/// Flatten an LSP [`SignatureHelp`] to the active signature and the byte span of
/// its active parameter within the label. Resolves both parameter-label forms
/// (a literal substring, or clangd's `[startUtf16, endUtf16]` offsets) and the
/// signature- vs request-level `activeParameter`. `None` if there are no
/// signatures.
pub fn active_signature_hint(help: &SignatureHelp) -> Option<SignatureHint> {
    let idx = help.active_signature.unwrap_or(0) as usize;
    let sig = help.signatures.get(idx).or_else(|| help.signatures.first())?;
    let label = sig.label.clone();
    let active_idx = sig.active_parameter.or(help.active_parameter);
    let active_param = active_idx
        .and_then(|ai| sig.parameters.as_ref()?.get(ai as usize))
        .and_then(|p| param_byte_range(&label, &p.label));
    Some(SignatureHint { label, active_param })
}

/// Resolve a [`ParameterLabel`] to a byte range within the signature `label`.
fn param_byte_range(label: &str, plabel: &ParameterLabel) -> Option<std::ops::Range<usize>> {
    match plabel {
        ParameterLabel::Simple(s) => {
            let start = label.find(s.as_str())?;
            Some(start..start + s.len())
        }
        ParameterLabel::LabelOffsets([a, b]) => {
            let start = utf16_offset_to_byte(label, *a as usize);
            let end = utf16_offset_to_byte(label, *b as usize);
            (start <= end && end <= label.len()).then_some(start..end)
        }
    }
}

/// UTF-16 code-unit offset → byte offset within `s` (LSP counts label offsets in
/// UTF-16, like positions). Clamps to `s.len()` for an out-of-range offset.
fn utf16_offset_to_byte(s: &str, utf16_idx: usize) -> usize {
    let mut u = 0;
    for (b, ch) in s.char_indices() {
        if u >= utf16_idx {
            return b;
        }
        u += ch.len_utf16();
    }
    s.len()
}

#[cfg(test)]
mod signature_tests {
    use super::*;
    use lsp_types::SignatureInformation;

    fn help(label: &str, params: Vec<ParameterLabel>, active: Option<u32>) -> SignatureHelp {
        SignatureHelp {
            signatures: vec![SignatureInformation {
                label: label.to_string(),
                documentation: None,
                parameters: Some(
                    params
                        .into_iter()
                        .map(|label| ParameterInformation { label, documentation: None })
                        .collect(),
                ),
                active_parameter: None,
            }],
            active_signature: Some(0),
            active_parameter: active,
        }
    }

    #[test]
    fn offsets_form_marks_active_param() {
        // "Point(int x, int y)" — the second param "int y" spans utf16 [13,18).
        let h = help(
            "Point(int x, int y)",
            vec![ParameterLabel::LabelOffsets([6, 11]), ParameterLabel::LabelOffsets([13, 18])],
            Some(1),
        );
        let hint = active_signature_hint(&h).unwrap();
        assert_eq!(hint.label, "Point(int x, int y)");
        assert_eq!(hint.active_param, Some(13..18));
        assert_eq!(&hint.label[hint.active_param.unwrap()], "int y");
    }

    #[test]
    fn simple_substring_form_is_located() {
        let h = help(
            "sum(int a, int b)",
            vec![ParameterLabel::Simple("int a".into()), ParameterLabel::Simple("int b".into())],
            Some(0),
        );
        let hint = active_signature_hint(&h).unwrap();
        assert_eq!(&hint.label[hint.active_param.unwrap()], "int a");
    }

    #[test]
    fn no_active_parameter_still_returns_label() {
        let h = help("foo()", vec![], None);
        let hint = active_signature_hint(&h).unwrap();
        assert_eq!(hint.label, "foo()");
        assert_eq!(hint.active_param, None);
    }
}
