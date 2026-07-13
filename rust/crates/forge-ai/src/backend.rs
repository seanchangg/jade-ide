//! The `InlineCompletionBackend` — lifecycle management (endpoint resolution,
//! managed `llama-server` supervision) and the single-flight `/infill` client.
//! Port of the class in `src/main/inline-completion.ts`.
//!
//! Concurrency model (the Rust analogue of JS's single-threaded event loop):
//! - `lifecycle` (an async mutex) serializes `start`/`stop`/`set_model` and the
//!   managed-startup poll loop, standing in for the fact that the TS methods
//!   never ran concurrently.
//! - `stopped` (an atomic) lets `stop()` signal a cancellation that an
//!   in-progress `start()` health-probe loop observes between probes, exactly as
//!   the TS `this.stopped` flag did (TS :59, :234-235).
//! - `/infill` runs off the lifecycle lock and reads the current endpoint from
//!   the published status, so a slow managed health-probe never blocks typing.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{watch, Mutex as AsyncMutex};
use tokio::task::AbortHandle;

use crate::{AiModelId, AiState, AiStatus, InfillRequest, InfillResult};

// ── Constants (TS :15-29) ──
const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8012";
const MANAGED_PORT: u16 = 8630;
const HEALTH_TIMEOUT_MS: u64 = 800;
const STARTUP_DEADLINE_MS: u64 = 15 * 60_000; // generous: first run downloads the model
const REQUEST_TIMEOUT_MS: u64 = 4000;
const POLL_INTERVAL_MS: u64 = 1500;
/// GUI apps on macOS don't inherit the shell PATH, so probe common install dirs
/// (TS :28-29).
const EXTRA_BIN_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"];

fn managed_endpoint() -> String {
    format!("http://127.0.0.1:{MANAGED_PORT}")
}

/// Lifecycle state guarded by the async mutex (the TS instance fields
/// `proc` / `endpoint` / `modelId`, TS :32-38).
struct Lifecycle {
    /// The managed child, `Some` only when *we* spawned it (TS `this.proc`).
    proc: Option<Child>,
    /// Bumped on every managed spawn / stop so a stale poll loop from a prior
    /// run exits instead of clobbering the new server's status.
    startup_gen: u64,
    model_id: AiModelId,
}

struct Shared {
    status_tx: watch::Sender<AiStatus>,
    lifecycle: AsyncMutex<Lifecycle>,
    stopped: AtomicBool,
    /// The abort handle of the in-flight `/infill` task and its generation, so a
    /// superseding call can abort it (the `AbortController` port, TS :36, :122).
    inflight: std::sync::Mutex<Option<(u64, AbortHandle)>>,
    inflight_gen: AtomicU64,
    client: reqwest::Client,
    /// Per-request `/infill` timeout (TS `REQUEST_TIMEOUT_MS`, :26). Field rather
    /// than constant only so tests can shorten it without a multi-second wait.
    request_timeout: Duration,
}

/// AI inline-completion backend. Cheap to clone is *not* implied — hold one and
/// share `&self`; internal state is `Arc`-wrapped for the poll/infill tasks.
pub struct InlineCompletionBackend {
    shared: Arc<Shared>,
}

impl InlineCompletionBackend {
    /// Construct in the `disabled` / "Not started" state (TS :34).
    pub fn new() -> Self {
        Self::with_request_timeout(Duration::from_millis(REQUEST_TIMEOUT_MS))
    }

    /// Like [`new`](Self::new) but with a custom `/infill` timeout. Exposed for
    /// tests that need to exercise the timeout path without a multi-second wait;
    /// production code should use [`new`](Self::new).
    #[doc(hidden)]
    pub fn with_request_timeout(request_timeout: Duration) -> Self {
        let (status_tx, _rx) = watch::channel(AiStatus::new(AiState::Disabled, "Not started"));
        let client = reqwest::Client::builder()
            .build()
            .expect("reqwest client builds with rustls");
        InlineCompletionBackend {
            shared: Arc::new(Shared {
                status_tx,
                lifecycle: AsyncMutex::new(Lifecycle {
                    proc: None,
                    startup_gen: 0,
                    model_id: AiModelId::default(),
                }),
                stopped: AtomicBool::new(false),
                inflight: std::sync::Mutex::new(None),
                inflight_gen: AtomicU64::new(0),
                client,
                request_timeout,
            }),
        }
    }

    /// Subscribe to status changes (analogue of `onStatus`, TS :113-115). The
    /// receiver's initial value is the current status.
    pub fn subscribe(&self) -> watch::Receiver<AiStatus> {
        self.shared.status_tx.subscribe()
    }

    /// The current status (TS `getStatus`, :109-111).
    pub fn status(&self) -> AiStatus {
        self.shared.status_tx.borrow().clone()
    }

    /// Resolve an endpoint or spawn the managed server. Idempotent — a second
    /// call while running or mid-startup is a no-op (TS :47-77).
    pub async fn start(&self) {
        let shared = &self.shared;
        let mut life = shared.lifecycle.lock().await;

        // Idempotent guard (TS :49).
        let state = shared.status_tx.borrow().state;
        if life.proc.is_some() || state == AiState::Ready || state == AiState::Starting {
            return;
        }
        shared.stopped.store(false, Ordering::SeqCst);

        shared.set_status(AiStatus::new(
            AiState::Starting,
            "Looking for a completion server…",
        ));

        // Candidates, in order (TS :52-53). Env var wins; empty/unset filtered.
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(ep) = std::env::var("JADE_FIM_ENDPOINT") {
            if !ep.is_empty() {
                candidates.push(ep);
            }
        }
        candidates.push(DEFAULT_ENDPOINT.to_string());
        candidates.push(managed_endpoint());

        for ep in candidates {
            let healthy = shared.is_healthy(&ep).await;
            if shared.stopped.load(Ordering::SeqCst) {
                return; // turned off while we were probing (TS :59)
            }
            if healthy {
                shared.set_status(AiStatus::ready(format!("Connected to {ep}"), ep));
                return;
            }
        }

        // Nothing running — spawn our own (TS :67-76).
        let Some(bin) = find_llama_server() else {
            shared.set_status(AiStatus::new(
                AiState::Disabled,
                "llama-server not found — `brew install llama.cpp`, or set \
                 JADE_FIM_ENDPOINT to a running server",
            ));
            return;
        };

        shared.spawn_managed(&mut life, &bin);
    }

    /// Kill the managed server (freeing GPU/unified memory) and abort any
    /// in-flight request. `start()` brings it back (TS :79-91).
    pub async fn stop(&self) {
        self.stop_with(String::from("Turned off")).await;
    }

    async fn stop_with(&self, detail: String) {
        let shared = &self.shared;
        // Set first, without the lock, so an in-progress start()'s probe loop
        // sees it immediately (TS sets `this.stopped = true` synchronously).
        shared.stopped.store(true, Ordering::SeqCst);
        shared.abort_inflight();

        let mut life = shared.lifecycle.lock().await;
        if let Some(mut child) = life.proc.take() {
            let _ = child.start_kill(); // SIGKILL; releases GPU memory (TS :85-87)
        }
        life.startup_gen = life.startup_gen.wrapping_add(1); // retire the poll loop
        shared.set_status(AiStatus::new(AiState::Disabled, detail));
    }

    /// `shutdown()` (TS :157-159) — stop with the shutdown detail.
    pub async fn shutdown(&self) {
        self.stop_with(String::from("Shutting down")).await;
    }

    /// Switch the managed model tier, restarting the server only if it's a child
    /// we spawned; adopted/external endpoints pick their own model (TS :96-107).
    pub async fn set_model(&self, id: AiModelId) {
        let shared = &self.shared;
        let restart = {
            let mut life = shared.lifecycle.lock().await;
            if id == life.model_id {
                return;
            }
            life.model_id = id;

            let using_managed = life.proc.is_some();
            let state = shared.status_tx.borrow().state;
            let running = state == AiState::Ready || state == AiState::Starting;
            if !running {
                return; // off/errored — applies on next start (TS :102)
            }
            if !using_managed && state == AiState::Ready {
                return; // external endpoint manages its own model (TS :103)
            }
            true
        };

        if restart {
            self.stop_with(String::from("Switching model…")).await;
            self.start().await;
        }
    }

    /// POST `/infill`. Single-flight: a new call aborts the in-flight one so the
    /// server's single slot isn't stuck on a stale suggestion (TS :117-155).
    /// Returns `None` on any error, non-2xx, timeout, or abort.
    pub async fn infill(&self, req: &InfillRequest) -> Option<InfillResult> {
        let shared = &self.shared;

        // Only serve when a resolved endpoint is ready (TS :118).
        let status = shared.status_tx.borrow().clone();
        if status.state != AiState::Ready {
            return None;
        }
        let endpoint = status.endpoint?;

        // Cancel whatever is still running (TS :122-124).
        shared.abort_inflight();

        let gen = shared.inflight_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let body = infill_body(req);
        let url = format!("{endpoint}/infill");
        let client = shared.client.clone();
        let request_timeout = shared.request_timeout;

        // Run the request as a task so a superseding call's `abort()` cancels the
        // reqwest future (dropping it), mirroring `controller.abort()`. The 4s
        // timeout stands in for the TS `setTimeout(() => controller.abort())`.
        let handle = tokio::spawn(async move {
            let resp = client
                .post(&url)
                .json(&body)
                .timeout(request_timeout)
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None; // `if (!res.ok) return null` (TS :145)
            }
            let json: serde_json::Value = resp.json().await.ok()?;
            // `typeof json.content === 'string' ? {content} : null` (TS :147)
            json.get("content")
                .and_then(|c| c.as_str())
                .map(|s| InfillResult {
                    content: s.to_string(),
                })
        });

        *shared.inflight.lock().unwrap() = Some((gen, handle.abort_handle()));

        // JoinError => aborted/superseded/panicked => no suggestion (TS :148-150).
        let out = handle.await.unwrap_or(None);

        // `if (this.inflight === controller) this.inflight = null` (TS :153).
        let mut guard = shared.inflight.lock().unwrap();
        if guard.as_ref().map(|(g, _)| *g) == Some(gen) {
            *guard = None;
        }
        out
    }
}

impl Default for InlineCompletionBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    fn set_status(&self, status: AiStatus) {
        // `send_replace` keeps the latest value even with no active receivers,
        // matching the TS field assignment `this.status = s` (TS :162).
        let _ = self.status_tx.send_replace(status);
    }

    fn abort_inflight(&self) {
        if let Some((_, handle)) = self.inflight.lock().unwrap().take() {
            handle.abort();
        }
    }

    /// GET `/health` with an 800ms timeout (TS :166-175).
    async fn is_healthy(&self, endpoint: &str) -> bool {
        match self
            .client
            .get(format!("{endpoint}/health"))
            .timeout(Duration::from_millis(HEALTH_TIMEOUT_MS))
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Spawn the managed `llama-server` and start the readiness poll (TS :193-249).
    fn spawn_managed(self: &Arc<Self>, life: &mut Lifecycle, bin: &str) {
        let endpoint = managed_endpoint();
        let model = life.model_id;

        self.set_status(AiStatus::new(
            AiState::Starting,
            format!("Starting {} (downloads on first use)…", model.label()),
        ));

        // Spawn args, verbatim (TS :201-210).
        let mut cmd = Command::new(bin);
        cmd.args([
            "-hf",
            model.hf(),
            "--host",
            "127.0.0.1",
            "--port",
            &MANAGED_PORT.to_string(),
            "-ngl",
            "99", // full GPU offload (Metal)
            "-ub",
            "1024",
            "-b",
            "1024",
            "--ctx-size",
            "0", // use the model's full context window
            "--cache-reuse",
            "256", // KV-prefix reuse — the JetBrains-style cache trick
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => {
                // `this.proc.on('error')` (TS :217-220).
                self.set_status(AiStatus::new(
                    AiState::Error,
                    format!("Failed to launch llama-server: {err}"),
                ));
                return;
            }
        };

        // stderr tail capture — last 2000 chars for error reporting (TS :212-215).
        let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(mut stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut t = tail.lock().unwrap();
                            t.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if t.len() > 2000 {
                                let cut = t.len() - 2000;
                                *t = t[cut..].to_string();
                            }
                        }
                    }
                }
            });
        }

        life.startup_gen = life.startup_gen.wrapping_add(1);
        let gen = life.startup_gen;
        life.proc = Some(child);

        // Poll until the model is loaded (server returns 503 while loading),
        // first tick after 1500ms (TS :233-248).
        let shared = self.clone();
        let deadline = Instant::now() + Duration::from_millis(STARTUP_DEADLINE_MS);
        tokio::spawn(async move {
            shared.poll_ready(gen, endpoint, model, deadline, stderr_tail).await;
        });
    }

    async fn poll_ready(
        self: Arc<Self>,
        gen: u64,
        endpoint: String,
        model: AiModelId,
        deadline: Instant,
        stderr_tail: Arc<std::sync::Mutex<String>>,
    ) {
        loop {
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            if self.stopped.load(Ordering::SeqCst) {
                return;
            }

            let mut life = self.lifecycle.lock().await;
            if life.startup_gen != gen {
                return; // superseded by a restart/stop
            }
            if life.proc.is_none() {
                return;
            }

            // Detect an early exit before health comes up (TS :222-230).
            if let Some(child) = life.proc.as_mut() {
                if let Ok(Some(exit)) = child.try_wait() {
                    life.proc = None;
                    let ready = self.status_tx.borrow().state == AiState::Ready;
                    if !self.stopped.load(Ordering::SeqCst) && !ready {
                        let tail = stderr_tail.lock().unwrap().clone();
                        let last3 = tail
                            .split('\n')
                            .filter(|l| !l.is_empty())
                            .collect::<Vec<_>>();
                        let last3 = last3[last3.len().saturating_sub(3)..].join(" ");
                        let code = exit.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
                        self.set_status(AiStatus::new(
                            AiState::Error,
                            format!("llama-server exited with code {code}: {last3}"),
                        ));
                    }
                    return;
                }
            }

            if self.is_healthy(&endpoint).await {
                self.set_status(AiStatus::ready(
                    format!("Local model ready ({})", model.label()),
                    endpoint.clone(),
                ));
                return;
            }

            if Instant::now() > deadline {
                // `setStatus(error)` then `this.shutdown()` (TS :242-243).
                self.set_status(AiStatus::new(
                    AiState::Error,
                    "Timed out waiting for the completion model to load",
                ));
                self.stopped.store(true, Ordering::SeqCst);
                if let Some(mut child) = life.proc.take() {
                    let _ = child.start_kill();
                }
                life.startup_gen = life.startup_gen.wrapping_add(1);
                self.set_status(AiStatus::new(AiState::Disabled, "Shutting down"));
                return;
            }
        }
    }
}

/// Build the `/infill` request body exactly as the TS did (TS :132-142).
fn infill_body(req: &InfillRequest) -> serde_json::Value {
    serde_json::json!({
        "input_prefix": req.prefix,
        "input_suffix": req.suffix,
        "n_predict": if req.single_line { 64 } else { 96 },
        "stop": if req.single_line { vec!["\n"] } else { Vec::<&str>::new() },
        "top_k": 40,
        "top_p": 0.99,
        "temperature": 0.1,
        "cache_prompt": true,
        "t_max_predict_ms": 1500,
    })
}

/// Locate the `llama-server` binary (TS :177-191):
/// `JADE_LLAMA_SERVER` (must exist) → each PATH dir → the extra install dirs,
/// taking the first that exists and is executable (X_OK).
fn find_llama_server() -> Option<String> {
    if let Ok(explicit) = std::env::var("JADE_LLAMA_SERVER") {
        if !explicit.is_empty() && Path::new(&explicit).exists() {
            return Some(explicit);
        }
    }

    let path_dirs = std::env::var("PATH").unwrap_or_default();
    let dirs = path_dirs
        .split(':')
        .map(|s| s.to_string())
        .chain(EXTRA_BIN_DIRS.iter().map(|s| s.to_string()));

    for dir in dirs {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(&dir).join("llama-server");
        if is_executable(&candidate) {
            return candidate.to_str().map(|s| s.to_string());
        }
    }
    None
}

/// `fs.accessSync(candidate, X_OK)` equivalent (TS :185-188).
fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}
