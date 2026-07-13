//! Jade AI inline-completion backend — Rust port of
//! `src/main/inline-completion.ts` (the Electron main-process class
//! `InlineCompletionBackend`).
//!
//! The backend speaks llama.cpp's HTTP `/infill` (fill-in-the-middle) + `/health`
//! API. It resolves an endpoint in three steps and, failing that, spawns and
//! supervises its own `llama-server` child process (TS :7-13):
//!   1. `JADE_FIM_ENDPOINT` env var (user-managed server, any host)
//!   2. `http://127.0.0.1:8012` (llama.vscode convention — adopt if running)
//!   3. `http://127.0.0.1:8630` (our managed port — adopts a crashed run's orphan)
//!   4. spawn our own `llama-server` with a small FIM model (downloads on first run)
//!
//! See `docs/jade-feature-inventory.md` §8.7 for the preserved contract. This is
//! a "thin client" identity feature: any server honoring `/health` +
//! `/infill {content}` can replace the managed one via `JADE_FIM_ENDPOINT`.
//!
//! No GUI dependencies: status is published over a [`tokio::sync::watch`] channel
//! (the analogue of the TS `onStatus` listener list, TS :113-115) for Phase-3
//! wiring to consume.

mod backend;

pub use backend::InlineCompletionBackend;

use serde::{Deserialize, Serialize};

/// Managed-model tiers — all FIM-tuned Qwen2.5-Coder GGUFs served by llama.cpp.
/// Mirrors `AI_MODELS` / `AiModelId` (TS :19-23, shared/types.ts :281).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiModelId {
    /// Qwen2.5-Coder 1.5B — the default tier (`private modelId = 'fast'`, TS :20, :38).
    #[default]
    Fast,
    /// Qwen2.5-Coder 3B (TS :21).
    Balanced,
    /// Qwen2.5-Coder 7B (TS :22).
    Best,
}

impl AiModelId {
    /// The HuggingFace repo passed to `llama-server -hf` (TS :20-22).
    pub fn hf(self) -> &'static str {
        match self {
            AiModelId::Fast => "ggml-org/Qwen2.5-Coder-1.5B-Q8_0-GGUF",
            AiModelId::Balanced => "ggml-org/Qwen2.5-Coder-3B-Q8_0-GGUF",
            AiModelId::Best => "ggml-org/Qwen2.5-Coder-7B-Q8_0-GGUF",
        }
    }

    /// Human-readable label used in status detail strings (TS :20-22).
    pub fn label(self) -> &'static str {
        match self {
            AiModelId::Fast => "Qwen2.5-Coder 1.5B",
            AiModelId::Balanced => "Qwen2.5-Coder 3B",
            AiModelId::Best => "Qwen2.5-Coder 7B",
        }
    }
}

/// The four completion states (shared/types.ts :278 `AiCompletionState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiState {
    Disabled,
    Starting,
    Ready,
    Error,
}

/// Status snapshot broadcast on the watch channel — port of the `AiStatus`
/// interface (shared/types.ts :283-287). `endpoint` is populated only in the
/// `Ready` state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiStatus {
    pub state: AiState,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl AiStatus {
    fn new(state: AiState, detail: impl Into<String>) -> Self {
        AiStatus {
            state,
            detail: detail.into(),
            endpoint: None,
        }
    }

    fn ready(detail: impl Into<String>, endpoint: impl Into<String>) -> Self {
        AiStatus {
            state: AiState::Ready,
            detail: detail.into(),
            endpoint: Some(endpoint.into()),
        }
    }
}

/// A fill-in-the-middle request — port of `InfillRequest` (shared/types.ts
/// :289-296). `single_line` maps to the TS `singleLine` (default `false`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InfillRequest {
    pub prefix: String,
    pub suffix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// Single-line mode: the server stops at the first newline — lower latency,
    /// CLion-style one-line ghost text (shared/types.ts :293-295).
    #[serde(default)]
    pub single_line: bool,
}

/// The one field we read back from a `/infill` response — port of
/// `InfillResult` (shared/types.ts :298-300).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfillResult {
    pub content: String,
}
