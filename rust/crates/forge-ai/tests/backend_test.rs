//! Integration tests for `InlineCompletionBackend` — no llama.cpp required.
//!
//! A tiny hand-rolled HTTP/1.1 server (raw `tokio::net::TcpListener`) on an
//! ephemeral port stands in for `llama-server`, serving `/health` and `/infill`.
//! It records the last `/infill` body for parity assertions and supports a
//! configurable per-request delay to exercise single-flight abort and timeout.

// `ENV_LOCK` is deliberately held across `.await` to serialize the process-global
// `JADE_FIM_ENDPOINT` mutation with the `start()` that reads it.
#![allow(clippy::await_holding_lock)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_ai::{AiModelId, AiState, InfillRequest, InlineCompletionBackend, ModelSource};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A configurable mock llama-server.
struct MockServer {
    endpoint: String,
    /// Last JSON body received on `/infill`.
    last_infill: Arc<Mutex<Option<Value>>>,
    /// Count of `/infill` requests actually received.
    infill_hits: Arc<AtomicU64>,
}

/// Per-endpoint behaviour knobs.
#[derive(Clone, Copy)]
struct MockConfig {
    /// Delay before responding to `/infill` (simulates generation latency).
    infill_delay: Duration,
    /// If false, `/health` returns 503 (server "loading").
    healthy: bool,
}

impl Default for MockConfig {
    fn default() -> Self {
        MockConfig {
            infill_delay: Duration::ZERO,
            healthy: true,
        }
    }
}

impl MockServer {
    async fn start(cfg: MockConfig) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://127.0.0.1:{}", addr.port());
        let last_infill = Arc::new(Mutex::new(None));
        let infill_hits = Arc::new(AtomicU64::new(0));

        let li = last_infill.clone();
        let hits = infill_hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let li = li.clone();
                let hits = hits.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(stream, cfg, li, hits).await;
                });
            }
        });

        MockServer {
            endpoint,
            last_infill,
            infill_hits,
        }
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    cfg: MockConfig,
    last_infill: Arc<Mutex<Option<Value>>>,
    infill_hits: Arc<AtomicU64>,
) -> std::io::Result<()> {
    // Read until end of headers, then the body per Content-Length.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let request_line = head.lines().next().unwrap_or("").to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            let l = l.to_ascii_lowercase();
            l.strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
        })
        .unwrap_or(0);

    // Read remaining body bytes.
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = &buf[header_end..(header_end + content_length).min(buf.len())];

    if request_line.starts_with("GET /health") {
        let resp = if cfg.healthy {
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
        } else {
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"
        };
        stream.write_all(resp.as_bytes()).await?;
    } else if request_line.starts_with("POST /infill") {
        infill_hits.fetch_add(1, Ordering::SeqCst);
        if let Ok(json) = serde_json::from_slice::<Value>(body) {
            *last_infill.lock().unwrap() = Some(json);
        }
        if !cfg.infill_delay.is_zero() {
            tokio::time::sleep(cfg.infill_delay).await;
        }
        let payload = r#"{"content":"completion"}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        stream.write_all(resp.as_bytes()).await?;
    } else {
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .await?;
    }
    let _ = stream.flush().await;
    Ok(())
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Wait until the backend reaches `Ready` (or fail after a bound).
async fn wait_ready(backend: &InlineCompletionBackend) {
    for _ in 0..50 {
        if backend.status().state == AiState::Ready {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("backend did not become ready: {:?}", backend.status());
}

// Env mutation is process-global; serialize the tests that touch JADE_FIM_ENDPOINT.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Take `ENV_LOCK`, ignoring poison. One failing test used to poison the mutex
/// and every other env test then failed with `PoisonError` instead of its own
/// result, which hides the one real failure behind six invented ones.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[tokio::test]
async fn resolves_env_endpoint_and_reports_ready() {
    let _guard = env_lock();
    let mock = MockServer::start(MockConfig::default()).await;

    std::env::set_var("JADE_FIM_ENDPOINT", &mock.endpoint);
    let backend = InlineCompletionBackend::new();

    // Status transitions: disabled -> starting -> ready.
    assert_eq!(backend.status().state, AiState::Disabled);
    backend.start().await;
    std::env::remove_var("JADE_FIM_ENDPOINT");

    assert_eq!(backend.status().state, AiState::Ready);
    let status = backend.status();
    assert_eq!(status.endpoint.as_deref(), Some(mock.endpoint.as_str()));
    assert!(status.detail.contains(&mock.endpoint));
}

#[tokio::test]
async fn infill_request_body_matches_ts_contract() {
    let _guard = env_lock();
    let mock = MockServer::start(MockConfig::default()).await;
    std::env::set_var("JADE_FIM_ENDPOINT", &mock.endpoint);
    let backend = InlineCompletionBackend::new();
    backend.start().await;
    std::env::remove_var("JADE_FIM_ENDPOINT");
    wait_ready(&backend).await;

    // Single-line request.
    let req = InfillRequest {
        prefix: "fn main() {".into(),
        suffix: "}".into(),
        filename: Some("main.rs".into()),
        single_line: true,
    };
    let out = backend.infill(&req).await;
    assert_eq!(out.unwrap().content, "completion");

    let body = mock.last_infill.lock().unwrap().clone().unwrap();
    assert_eq!(body["input_prefix"], "fn main() {");
    assert_eq!(body["input_suffix"], "}");
    assert_eq!(body["n_predict"], 64);
    assert_eq!(body["stop"], serde_json::json!(["\n"]));
    assert_eq!(body["top_k"], 40);
    assert_eq!(body["top_p"], 0.99);
    assert_eq!(body["temperature"], 0.1);
    assert_eq!(body["cache_prompt"], true);
    assert_eq!(body["t_max_predict_ms"], 1500);

    // Multi-line request: n_predict 96, and it stops at a blank line — the
    // client cuts there anyway, so generating past it only costs latency.
    let req2 = InfillRequest {
        prefix: "a".into(),
        suffix: "b".into(),
        filename: None,
        single_line: false,
    };
    backend.infill(&req2).await.unwrap();
    let body2 = mock.last_infill.lock().unwrap().clone().unwrap();
    assert_eq!(body2["n_predict"], 96);
    assert_eq!(body2["stop"], serde_json::json!(["\n\n"]));
}

#[tokio::test]
async fn single_flight_aborts_the_in_flight_request() {
    let _guard = env_lock();
    // First /infill is slow; a superseding call should abort it -> None.
    let mock = MockServer::start(MockConfig {
        infill_delay: Duration::from_millis(1500),
        healthy: true,
    })
    .await;
    std::env::set_var("JADE_FIM_ENDPOINT", &mock.endpoint);
    let backend = Arc::new(InlineCompletionBackend::new());
    backend.start().await;
    std::env::remove_var("JADE_FIM_ENDPOINT");
    wait_ready(&backend).await;

    let req = InfillRequest {
        prefix: "x".into(),
        suffix: "y".into(),
        filename: None,
        single_line: true,
    };

    let b1 = backend.clone();
    let r1 = req.clone();
    let first = tokio::spawn(async move { b1.infill(&r1).await });

    // Let the first request reach the server, then supersede it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let second = backend.infill(&req).await;

    let first = first.await.unwrap();
    assert!(first.is_none(), "superseded request should return None");
    assert_eq!(second.unwrap().content, "completion");
    // Both requests reached the server; the first was aborted client-side.
    assert_eq!(mock.infill_hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn infill_times_out_to_none() {
    let _guard = env_lock();
    // Response arrives well after the (shortened) request timeout.
    let mock = MockServer::start(MockConfig {
        infill_delay: Duration::from_millis(800),
        healthy: true,
    })
    .await;
    std::env::set_var("JADE_FIM_ENDPOINT", &mock.endpoint);
    let backend = InlineCompletionBackend::with_request_timeout(Duration::from_millis(150));
    backend.start().await;
    std::env::remove_var("JADE_FIM_ENDPOINT");
    wait_ready(&backend).await;

    let req = InfillRequest {
        prefix: "x".into(),
        suffix: "y".into(),
        filename: None,
        single_line: true,
    };
    assert!(backend.infill(&req).await.is_none());
}

#[tokio::test]
async fn infill_returns_none_when_not_ready() {
    let backend = InlineCompletionBackend::new();
    let req = InfillRequest {
        prefix: "x".into(),
        suffix: "y".into(),
        filename: None,
        single_line: true,
    };
    // Never started -> disabled -> None.
    assert_eq!(backend.status().state, AiState::Disabled);
    assert!(backend.infill(&req).await.is_none());
}

#[tokio::test]
async fn stop_transitions_to_disabled_and_aborts() {
    let _guard = env_lock();
    let mock = MockServer::start(MockConfig::default()).await;
    std::env::set_var("JADE_FIM_ENDPOINT", &mock.endpoint);
    let backend = InlineCompletionBackend::new();
    backend.start().await;
    std::env::remove_var("JADE_FIM_ENDPOINT");
    wait_ready(&backend).await;

    backend.stop().await;
    let status = backend.status();
    assert_eq!(status.state, AiState::Disabled);
    assert_eq!(status.detail, "Turned off");
    assert!(status.endpoint.is_none());
    // Not ready anymore -> infill is a no-op.
    let req = InfillRequest {
        prefix: "x".into(),
        suffix: "y".into(),
        filename: None,
        single_line: true,
    };
    assert!(backend.infill(&req).await.is_none());
}

#[tokio::test]
async fn start_is_idempotent() {
    let _guard = env_lock();
    let mock = MockServer::start(MockConfig::default()).await;
    std::env::set_var("JADE_FIM_ENDPOINT", &mock.endpoint);
    let backend = InlineCompletionBackend::new();
    backend.start().await;
    backend.start().await; // second call while ready is a no-op
    std::env::remove_var("JADE_FIM_ENDPOINT");
    assert_eq!(backend.status().state, AiState::Ready);
}

/// The sprite tier serves a GGUF off disk rather than a HuggingFace repo, and
/// asks for a bigger batch because llama-server derives the kept prefix from
/// it (`n_prefix_take = 3*(n_batch/4)`; the model trains on 1280).
#[test]
fn sprite_tier_serves_a_local_gguf() {
    let _guard = env_lock();
    std::env::set_var("FORGE_SPRITE_MODEL", "/tmp/whatever/sprite.gguf");
    assert_eq!(
        AiModelId::Sprite.source(),
        ModelSource::Local(std::path::PathBuf::from("/tmp/whatever/sprite.gguf")),
        "FORGE_SPRITE_MODEL redirects the sprite tier"
    );
    std::env::remove_var("FORGE_SPRITE_MODEL");

    assert!(matches!(AiModelId::Fastest.source(), ModelSource::Hf(_)),
            "the Qwen tiers still download");
    assert_eq!(AiModelId::Sprite.batch(), 1792);
    assert_eq!(AiModelId::Fastest.batch(), 1024, "other tiers keep the tuned 1024");
    assert_eq!(AiModelId::Sprite.label(), "sprite-100m");

    // ai.json stores the tier by name; `sprite` must round-trip.
    let json = serde_json::to_string(&AiModelId::Sprite).unwrap();
    assert_eq!(json, "\"sprite\"");
    assert_eq!(
        serde_json::from_str::<AiModelId>(&json).unwrap(),
        AiModelId::Sprite
    );

    // The model was called `ghost` until 2026-08-09. An ai.json written before
    // the rename must still load, or the tier silently drops to the default.
    assert_eq!(
        serde_json::from_str::<AiModelId>("\"ghost\"").unwrap(),
        AiModelId::Sprite,
        "the old name stays readable"
    );
}

// ── Quitting Jade must stop the managed server ────────────────────────────────
//
// The bug these cover: nothing killed llama-server when the app quit. The tokio
// `Child` carries `kill_on_drop`, but GPUI ends the process without dropping it,
// so the server survived its parent, held the GPU, and kept answering on the
// managed port. `kill_managed_now()` is the synchronous kill the quit and signal
// handlers call.

/// A stand-in for llama-server that runs until something kills it.
fn fake_server_script(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-llama-server");
    std::fs::write(&path, "#!/bin/sh\nexec sleep 300\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// `ps` state for `pid`: `None` once it is gone, `Some("Z…")` while it is a
/// killed-but-unreaped zombie (we still hold the `Child`, so nothing has waited
/// on it yet). Either one proves the signal landed.
fn process_state(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if state.is_empty() {
        None
    } else {
        Some(state)
    }
}

fn assert_dead(pid: u32, what: &str) {
    for _ in 0..200 {
        match process_state(pid) {
            None => return,
            Some(s) if s.starts_with('Z') => return,
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    panic!("{what}: pid {pid} is still running ({:?})", process_state(pid));
}

#[tokio::test]
async fn kill_managed_now_stops_the_server_and_clears_the_pid_file() {
    let _guard = env_lock();
    let dir = std::env::temp_dir().join(format!("jade-ai-kill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = fake_server_script(&dir);
    let pid_file = dir.join("llama-server.pid");
    std::env::set_var("JADE_LLAMA_PIDFILE", &pid_file);

    let backend = InlineCompletionBackend::new();
    backend.spawn_managed_for_test(bin.to_str().unwrap()).await;

    let pid = backend.managed_pid().expect("the spawn records a pid");
    assert_eq!(
        std::fs::read_to_string(&pid_file).unwrap().trim(),
        pid.to_string(),
        "the pid file names the server, so the next run can adopt an orphan"
    );
    assert!(process_state(pid).is_some(), "the fake server is running");

    // The quit path: synchronous, no runtime, no lifecycle lock.
    assert!(backend.kill_managed_now(), "the signal goes out");
    assert_dead(pid, "kill_managed_now");

    assert!(!pid_file.exists(), "a killed server leaves no pid file behind");
    assert_eq!(backend.managed_pid(), None);
    assert!(
        !backend.kill_managed_now(),
        "a second call has nothing to kill"
    );

    std::env::remove_var("JADE_LLAMA_PIDFILE");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stop_kills_the_server_by_pid_as_well_as_by_handle() {
    let _guard = env_lock();
    let dir = std::env::temp_dir().join(format!("jade-ai-stop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = fake_server_script(&dir);
    let pid_file = dir.join("llama-server.pid");
    std::env::set_var("JADE_LLAMA_PIDFILE", &pid_file);

    let backend = InlineCompletionBackend::new();
    backend.spawn_managed_for_test(bin.to_str().unwrap()).await;
    let pid = backend.managed_pid().expect("the spawn records a pid");

    backend.stop().await;
    assert_dead(pid, "stop");
    assert_eq!(backend.status().state, AiState::Disabled);
    assert!(!pid_file.exists());

    std::env::remove_var("JADE_LLAMA_PIDFILE");
    let _ = std::fs::remove_dir_all(&dir);
}
