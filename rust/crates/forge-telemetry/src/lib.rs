//! Jade telemetry channel — Rust port of `src/main/telemetry-server.ts`.
//!
//! The probe dylib and instrumented training programs connect over a Unix
//! domain socket (path exported via `FORGE_TELEMETRY_SOCK`) and speak NDJSON.
//! See `docs/telemetry-protocol.md` for the authoritative wire spec and
//! `docs/jade-feature-inventory.md` §8.3 for preserved semantics.

mod protocol;
mod server;

pub use protocol::{ClientMessage, Decl, Kind, Scalar, Tensor, Timing, Track, DEFAULT_MAX_DIM};
pub use server::{Event, TelemetryServer};
