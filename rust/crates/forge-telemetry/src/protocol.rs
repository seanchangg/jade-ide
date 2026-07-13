//! Wire types for the Forge telemetry channel.
//!
//! Authoritative spec: `docs/telemetry-protocol.md`. Transport is NDJSON over a
//! Unix domain socket; unknown message types and malformed lines are skipped
//! silently, and clients are expected to do the same with `track` messages.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Scalar,
    Timer,
    Buffer,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Scalar => "scalar",
            Kind::Timer => "timer",
            Kind::Buffer => "buffer",
        }
    }
}

/// `decl` — declare an item. Optional; any name first seen via a data message
/// is auto-registered identically.
#[derive(Debug, Clone, Deserialize)]
pub struct Decl {
    pub kind: Kind,
    pub name: String,
    #[serde(default)]
    pub meta: Option<Map<String, Value>>,
}

/// `scalar` — a scalar sample. `t` is unix seconds; the server substitutes its
/// own receive time when omitted.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Scalar {
    pub name: String,
    pub step: i64,
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<f64>,
}

/// `timing` — an operation timing sample.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Timing {
    pub name: String,
    pub ms: f64,
    pub step: i64,
}

/// `tensor` — a GPU-buffer frame. `data` is base64 of row-major little-endian
/// f32, already downsampled by the client to at most `maxDim × maxDim`.
#[derive(Debug, Clone, Deserialize)]
pub struct Tensor {
    pub name: String,
    pub step: i64,
    pub rows: u32,
    pub cols: u32,
    pub dtype: String,
    pub data: String,
    #[serde(default, rename = "srcRows")]
    pub src_rows: Option<u32>,
    #[serde(default, rename = "srcCols")]
    pub src_cols: Option<u32>,
}

impl Tensor {
    /// Decode `data` into f32s. Returns `None` if the payload is not valid
    /// base64 or its length doesn't match `rows * cols` f32s.
    pub fn decode(&self) -> Option<Vec<f32>> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.data)
            .ok()?;
        let expected = self.rows as usize * self.cols as usize * 4;
        if bytes.len() != expected {
            return None;
        }
        Some(
            bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        )
    }
}

/// IDE → client `track` control message. `name` is always the client's own
/// name for the item (aliases are translated back before sending).
#[derive(Debug, Clone, Serialize)]
pub struct Track {
    #[serde(rename = "type")]
    pub msg_type: &'static str, // always "track"
    pub kind: Kind,
    pub name: String,
    pub enabled: bool,
    #[serde(rename = "maxDim")]
    pub max_dim: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cols: Option<u32>,
}

/// A parsed client → IDE message.
#[derive(Debug, Clone)]
pub enum ClientMessage {
    Decl(Decl),
    Scalar(Scalar),
    Timing(Timing),
    Tensor(Tensor),
}

impl ClientMessage {
    /// Parse one NDJSON line. Returns `None` for malformed JSON, missing/unknown
    /// `type`, or a recognized type whose fields don't validate — all skipped
    /// silently per the protocol's robustness rules.
    pub fn parse(line: &str) -> Option<ClientMessage> {
        let value: Value = serde_json::from_str(line).ok()?;
        let msg_type = value.get("type")?.as_str()?;
        match msg_type {
            "decl" => serde_json::from_value(value).ok().map(ClientMessage::Decl),
            "scalar" => serde_json::from_value(value).ok().map(ClientMessage::Scalar),
            "timing" => serde_json::from_value(value).ok().map(ClientMessage::Timing),
            "tensor" => serde_json::from_value(value).ok().map(ClientMessage::Tensor),
            _ => None,
        }
    }
}

pub const DEFAULT_MAX_DIM: u32 = 128;

/// Partial-line guard: reset the read buffer if a single line grows past this
/// (malformed-client protection; matches the TS server's 8 MB cap).
pub const MAX_PARTIAL_LINE: usize = 8 * 1024 * 1024;
