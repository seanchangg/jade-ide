//! The clangd LSP client: a direct thin JSON-RPC client over the server's
//! stdio. Faithful port of `src/main/lsp-client.ts` (the `LspClient` class,
//! lines 28-296) with one deliberate improvement — incremental `didChange`
//! (see [`DidChange`]).
//!
//! ## Architecture
//!
//! We follow the same single-owner actor shape as the LLDB driver
//! (`forge-debug`): all mutable protocol state lives in one tokio task, reached
//! only by message passing, so there are no locks on the hot path.
//!
//! ```text
//!   LspHandle  ──cmd_tx (mpsc)──►  actor task  ──stdin──►  clangd
//!       ▲                             ▲   │
//!       │                    incoming │   │ event_tx (mpsc)
//!       │                     (mpsc)  │   ▼
//!   caller (jade)              reader task ◄──stdout── clangd   LspEvent stream
//! ```
//!
//! * A **reader task** owns `stdout`, reassembles frames with
//!   [`crate::framing::FrameDecoder`], and forwards each message body to the
//!   actor. On EOF it drops its sender, which the actor reads as "server gone".
//! * The **actor task** owns `stdin`, the child handle, and the pending-request
//!   map (`id → oneshot`). It writes outgoing requests/notifications, routes
//!   responses back to their waiters, answers server-initiated requests with
//!   safe defaults so clangd never stalls, and forwards notifications
//!   (`publishDiagnostics`) as [`LspEvent`]s.
//! * A **stderr drain task** swallows clangd's log spew, exactly like the TS
//!   `stderr.on('data')` no-op handler (`lsp-client.ts:52-55`).
//!
//! ## Superseded completions
//!
//! Cancelling an in-flight completion when the user types another character is
//! the **caller's** concern: drop the returned future (or ignore its result)
//! and issue a new [`LspHandle::completion`]. We do not send `$/cancelRequest`;
//! each request is independent and guarded by [`REQUEST_TIMEOUT`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use lsp_types::{
    CompletionItem, CompletionResponse, Diagnostic, GotoDefinitionResponse, Hover, Location,
    Position, TextDocumentSyncCapability, TextDocumentSyncKind,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use crate::framing::{encode_message, FrameDecoder};
use crate::uri::{language_id, path_from_uri, uri_from_path};

/// Per-request timeout guard. A wedged clangd must never hang the UI thread, so
/// every round-trip request resolves — with data or [`LspError::Timeout`] —
/// within this bound. Generous because clangd's first completion after spawn
/// waits on background indexing.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// clangd spawn flags. Byte-for-byte the TS argument list
/// (`lsp-client.ts:41-47`).
const CLANGD_ARGS: &[&str] = &[
    "--background-index",
    "--clang-tidy",
    "--completion-style=detailed",
    "--header-insertion=iwyu",
    "--pch-storage=memory",
];

/// Errors surfaced by request methods.
#[derive(Debug)]
pub enum LspError {
    /// The server process is gone / the actor task has exited.
    ServerGone,
    /// No response within [`REQUEST_TIMEOUT`].
    Timeout,
    /// The server returned a JSON-RPC error object.
    Protocol(String),
    /// A response could not be deserialized into the expected type.
    Decode(String),
    /// Spawning clangd or the initialize handshake failed.
    Init(String),
    /// An incremental change was supplied but the server negotiated full-text
    /// sync — the caller must send [`DidChange::Full`] instead.
    FullSyncRequired,
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LspError::ServerGone => write!(f, "clangd is not running"),
            LspError::Timeout => write!(f, "clangd request timed out after {REQUEST_TIMEOUT:?}"),
            LspError::Protocol(m) => write!(f, "clangd protocol error: {m}"),
            LspError::Decode(m) => write!(f, "could not decode clangd response: {m}"),
            LspError::Init(m) => write!(f, "clangd initialize failed: {m}"),
            LspError::FullSyncRequired => {
                write!(f, "server negotiated full-text sync; send DidChange::Full")
            }
        }
    }
}

impl std::error::Error for LspError {}

pub type Result<T> = std::result::Result<T, LspError>;

/// A single UTF-16 range edit for incremental `didChange`. The `range`'s
/// `character` fields are UTF-16 code-unit offsets (LSP's native unit), which is
/// exactly what the `forge-buffer` crate emits — pass them straight through.
#[derive(Debug, Clone)]
pub struct Utf16RangeEdit {
    pub range: lsp_types::Range,
    pub text: String,
}

/// How a document changed since the last version. The **improvement over the TS
/// client** (which only ever sent `contentChanges` verbatim from the renderer):
/// we negotiate [`TextDocumentSyncKind`] and send incremental edits when the
/// server supports them, full text otherwise.
#[derive(Debug, Clone)]
pub enum DidChange {
    /// Incremental `(range, text)` edits. Valid only when the negotiated sync
    /// kind is [`TextDocumentSyncKind::INCREMENTAL`]; otherwise
    /// [`LspHandle::did_change`] returns [`LspError::FullSyncRequired`].
    Incremental(Vec<Utf16RangeEdit>),
    /// The full new document text. Always valid.
    Full(String),
}

/// Events pushed to jade over the event channel. Shape mirrors the TS
/// `onDiagnostics` forwarding (`lsp-client.ts:209-215, 248-253`) plus lifecycle
/// signals.
#[derive(Debug, Clone)]
pub enum LspEvent {
    /// Server finished the initialize handshake and is accepting requests.
    Ready,
    /// `textDocument/publishDiagnostics`, keyed by the affected file path.
    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
    /// clangd's stdout closed / the process exited. Terminal.
    Exited,
}

/// Namespace for the constructor. `LspClient::initialize` spawns clangd and
/// returns a live [`LspHandle`].
pub struct LspClient;

impl LspClient {
    /// Spawn clangd, run the initialize handshake, and return a handle. Port of
    /// `LspClient.initialize` (`lsp-client.ts:34-118`):
    /// * spawns `clangd <CLANGD_ARGS>` with `cwd = root`,
    /// * sends `initialize` with the exact capability set from the TS client and
    ///   `initializationOptions.fallbackFlags = ['-std=c++17', '-I<include>']`,
    /// * sends `initialized`.
    ///
    /// `include_dir` is the forge include directory (`idetools.h`); when `None`
    /// only `-std=c++17` is passed as a fallback flag. `initialize` is naturally
    /// idempotent per workspace at the caller level — drop the old handle (which
    /// kills clangd via `kill_on_drop`) and call again, matching the TS
    /// "kill existing instance" step (`lsp-client.ts:36`).
    pub async fn initialize(root: &Path, include_dir: Option<&Path>) -> Result<LspHandle> {
        let mut child = Command::new("clangd")
            .args(CLANGD_ARGS)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| LspError::Init(format!("spawn clangd: {e}")))?;

        let mut stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Drain stderr forever — clangd logs there; ignored like the TS no-op
        // (`lsp-client.ts:52-55`).
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut chunk = [0u8; 8192];
            while let Ok(n) = reader.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
            }
        });

        let mut reader = FramedReader::new(stdout);

        // ── initialize request (id 0) ──
        let root_uri = uri_from_path(root);
        let mut fallback_flags = vec![json!("-std=c++17")];
        if let Some(inc) = include_dir {
            fallback_flags.push(json!(format!("-I{}", inc.display())));
        }
        let workspace_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("workspace");

        // Capabilities are hand-built to mirror `lsp-client.ts:79-108` exactly.
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri.as_str(),
            "initializationOptions": { "fallbackFlags": fallback_flags },
            "capabilities": {
                "textDocument": {
                    "completion": {
                        "completionItem": {
                            "snippetSupport": true,
                            "commitCharactersSupport": true,
                            "documentationFormat": ["markdown", "plaintext"],
                            "resolveSupport": { "properties": ["documentation", "detail"] }
                        },
                        "contextSupport": true
                    },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": false },
                    "references": {},
                    "publishDiagnostics": { "relatedInformation": true },
                    "synchronization": {
                        "didSave": true,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                        "dynamicRegistration": false
                    }
                },
                "workspace": { "workspaceFolders": true }
            },
            "workspaceFolders": [{ "uri": root_uri.as_str(), "name": workspace_name }]
        });

        write_message(
            &mut stdin,
            &json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": init_params }),
        )
        .await
        .map_err(|e| LspError::Init(format!("write initialize: {e}")))?;

        // Read until the initialize response (id 0) arrives, answering any early
        // server request with a null default and ignoring notifications.
        let init_result = loop {
            let frame = tokio::time::timeout(REQUEST_TIMEOUT, reader.next())
                .await
                .map_err(|_| LspError::Init("timed out awaiting initialize response".into()))?
                .map_err(|e| LspError::Init(format!("read: {e}")))?
                .ok_or_else(|| LspError::Init("clangd closed stdout during handshake".into()))?;
            let msg: Value = serde_json::from_slice(&frame)
                .map_err(|e| LspError::Init(format!("bad JSON in handshake: {e}")))?;

            if msg.get("id").and_then(Value::as_u64) == Some(0) && msg.get("method").is_none() {
                if let Some(err) = msg.get("error") {
                    return Err(LspError::Init(format!("server rejected initialize: {err}")));
                }
                break msg.get("result").cloned().unwrap_or(Value::Null);
            }
            // Early server-initiated request during handshake: answer null so
            // clangd doesn't block.
            if let (Some(id), true) = (msg.get("id"), msg.get("method").is_some()) {
                let _ = write_message(
                    &mut stdin,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null }),
                )
                .await;
            }
        };

        // Negotiate the sync kind from ServerCapabilities.textDocumentSync.
        let sync_kind = parse_sync_kind(&init_result);

        // ── initialized notification (`lsp-client.ts:116`) ──
        write_message(
            &mut stdin,
            &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        )
        .await
        .map_err(|e| LspError::Init(format!("write initialized: {e}")))?;

        // Spin up reader + actor.
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        // Reader task: frames → actor. Drops incoming_tx on EOF.
        tokio::spawn(async move {
            let mut reader = reader;
            while let Ok(Some(frame)) = reader.next().await {
                if incoming_tx.send(frame).is_err() {
                    break; // actor gone
                }
            }
        });

        let actor = Actor {
            stdin,
            child,
            pending: HashMap::new(),
            next_id: 1,
            event_tx: event_tx.clone(),
        };
        tokio::spawn(actor.run(cmd_rx, incoming_rx));

        // Server is up.
        let _ = event_tx.send(LspEvent::Ready);

        Ok(LspHandle {
            cmd_tx,
            events: Some(event_rx),
            sync_kind,
        })
    }
}

/// Handle to a live clangd session. Cheaply usable from any task: request
/// methods borrow `&self` and route through the actor. Dropping the handle
/// drops `cmd_tx`; the actor then kills clangd (`kill_on_drop`).
pub struct LspHandle {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    events: Option<mpsc::UnboundedReceiver<LspEvent>>,
    sync_kind: TextDocumentSyncKind,
}

impl LspHandle {
    /// The `TextDocumentSyncKind` clangd negotiated. Callers consult this to
    /// decide between [`DidChange::Incremental`] and [`DidChange::Full`].
    pub fn sync_kind(&self) -> TextDocumentSyncKind {
        self.sync_kind
    }

    /// Take the event receiver (once). Jade drives this on its own task,
    /// forwarding [`LspEvent::Diagnostics`] to the editor. Returns `None` on a
    /// second call.
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<LspEvent>> {
        self.events.take()
    }

    // ── Document sync notifications (fire-and-forget) ──

    /// `textDocument/didOpen` (`lsp-client.ts:182-192`).
    pub fn did_open(&self, path: &Path, text: &str, version: i32) -> Result<()> {
        let uri = uri_from_path(path);
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri.as_str(),
                "languageId": language_id(path),
                "version": version,
                "text": text,
            }}),
        )
    }

    /// `textDocument/didChange` (`lsp-client.ts:194-200`), extended with
    /// incremental sync. When [`DidChange::Incremental`] is given but the server
    /// negotiated full sync, returns [`LspError::FullSyncRequired`].
    pub fn did_change(&self, path: &Path, change: DidChange, version: i32) -> Result<()> {
        let content_changes = match change {
            DidChange::Full(text) => vec![json!({ "text": text })],
            DidChange::Incremental(edits) => {
                if self.sync_kind == TextDocumentSyncKind::FULL {
                    return Err(LspError::FullSyncRequired);
                }
                edits
                    .into_iter()
                    .map(|e| json!({ "range": e.range, "text": e.text }))
                    .collect()
            }
        };
        let uri = uri_from_path(path);
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri.as_str(), "version": version },
                "contentChanges": content_changes,
            }),
        )
    }

    /// `textDocument/didSave` — enabled by `synchronization.didSave: true`
    /// (`lsp-client.ts:99`). Sends the notification with no text payload.
    pub fn did_save(&self, path: &Path) -> Result<()> {
        let uri = uri_from_path(path);
        self.notify(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri.as_str() } }),
        )
    }

    /// `textDocument/didClose` (`lsp-client.ts:202-207`).
    pub fn did_close(&self, path: &Path) -> Result<()> {
        let uri = uri_from_path(path);
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri.as_str() } }),
        )
    }

    // ── Requests (round-trip, guarded by REQUEST_TIMEOUT) ──

    /// `textDocument/completion` with `context.triggerKind = Invoked`
    /// (`lsp-client.ts:120-135`). Flattens the `CompletionList | CompletionItem[]`
    /// response to a plain item vec, matching the TS `{ items }` normalization.
    pub async fn completion(&self, path: &Path, position: Position) -> Result<Vec<CompletionItem>> {
        let uri = uri_from_path(path);
        let params = json!({
            "textDocument": { "uri": uri.as_str() },
            "position": position,
            "context": { "triggerKind": 1 }, // CompletionTriggerKind.Invoked
        });
        let raw = self.request("textDocument/completion", params).await?;
        if raw.is_null() {
            return Ok(Vec::new());
        }
        let resp: CompletionResponse =
            serde_json::from_value(raw).map_err(|e| LspError::Decode(e.to_string()))?;
        Ok(match resp {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        })
    }

    /// `textDocument/hover` (`lsp-client.ts:137-148`). `None` when the server
    /// has nothing to show at that position.
    pub async fn hover(&self, path: &Path, position: Position) -> Result<Option<Hover>> {
        let uri = uri_from_path(path);
        let params = json!({
            "textDocument": { "uri": uri.as_str() },
            "position": position,
        });
        let raw = self.request("textDocument/hover", params).await?;
        if raw.is_null() {
            return Ok(None);
        }
        serde_json::from_value(raw)
            .map(Some)
            .map_err(|e| LspError::Decode(e.to_string()))
    }

    /// `textDocument/definition` (`lsp-client.ts:150-164`). Normalizes the
    /// `Location | Location[] | LocationLink[]` response to a `Vec<Location>`
    /// (matching the TS array coercion; `linkSupport: false` means clangd
    /// returns plain `Location`s).
    pub async fn definition(&self, path: &Path, position: Position) -> Result<Vec<Location>> {
        let uri = uri_from_path(path);
        let params = json!({
            "textDocument": { "uri": uri.as_str() },
            "position": position,
        });
        let raw = self.request("textDocument/definition", params).await?;
        if raw.is_null() {
            return Ok(Vec::new());
        }
        let resp: GotoDefinitionResponse =
            serde_json::from_value(raw).map_err(|e| LspError::Decode(e.to_string()))?;
        Ok(match resp {
            GotoDefinitionResponse::Scalar(l) => vec![l],
            GotoDefinitionResponse::Array(v) => v,
            GotoDefinitionResponse::Link(links) => links
                .into_iter()
                .map(|l| Location::new(l.target_uri, l.target_selection_range))
                .collect(),
        })
    }

    /// `textDocument/references` with `context.includeDeclaration = true`
    /// (`lsp-client.ts:166-180`).
    pub async fn references(&self, path: &Path, position: Position) -> Result<Vec<Location>> {
        let uri = uri_from_path(path);
        let params = json!({
            "textDocument": { "uri": uri.as_str() },
            "position": position,
            "context": { "includeDeclaration": true },
        });
        let raw = self.request("textDocument/references", params).await?;
        if raw.is_null() {
            return Ok(Vec::new());
        }
        serde_json::from_value(raw).map_err(|e| LspError::Decode(e.to_string()))
    }

    /// The custom `memory-info` feature (inventory §8.5): hover at `position`
    /// and extract `sizeof = <n>` from the hover text. Port of the
    /// `LSP_MEMORY_INFO` handler (`lsp-client.ts:284-295`); returns the parsed
    /// byte size, or `None` when the hover has no `sizeof`.
    pub async fn memory_info(&self, path: &Path, position: Position) -> Result<Option<u64>> {
        let Some(hover) = self.hover(path, position).await? else {
            return Ok(None);
        };
        Ok(extract_sizeof(&flatten_hover(&hover)))
    }

    /// Graceful shutdown: `shutdown` request → `exit` notification → kill. Port
    /// of `shutdown` (`lsp-client.ts:217-232`). Consumes the handle.
    pub async fn shutdown(self) {
        // Best-effort shutdown request (ignore errors / timeout).
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", json!({}));
        let _ = self.cmd_tx.send(Cmd::Kill);
        // Give the actor a beat to kill the child, then drop cmd_tx.
    }

    fn notify(&self, method: &'static str, params: Value) -> Result<()> {
        self.cmd_tx
            .send(Cmd::Notify { method, params })
            .map_err(|_| LspError::ServerGone)
    }

    async fn request(&self, method: &'static str, params: Value) -> Result<Value> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Cmd::Request {
                method,
                params,
                reply,
            })
            .map_err(|_| LspError::ServerGone)?;
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(LspError::ServerGone), // actor dropped the oneshot
            Err(_) => Err(LspError::Timeout),
        }
    }
}

/// Control messages from a handle to the actor.
enum Cmd {
    Request {
        method: &'static str,
        params: Value,
        reply: oneshot::Sender<Result<Value>>,
    },
    Notify {
        method: &'static str,
        params: Value,
    },
    Kill,
}

/// The single owner of clangd's stdin, child handle, and pending-request map.
struct Actor {
    stdin: ChildStdin,
    child: Child,
    pending: HashMap<u64, oneshot::Sender<Result<Value>>>,
    next_id: u64,
    event_tx: mpsc::UnboundedSender<LspEvent>,
}

impl Actor {
    async fn run(
        mut self,
        mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
        mut incoming_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        loop {
            tokio::select! {
                biased;

                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(Cmd::Notify { method, params }) => {
                            let _ = write_message(
                                &mut self.stdin,
                                &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
                            ).await;
                        }
                        Some(Cmd::Request { method, params, reply }) => {
                            let id = self.next_id;
                            self.next_id += 1;
                            let msg = json!({
                                "jsonrpc": "2.0", "id": id, "method": method, "params": params
                            });
                            if write_message(&mut self.stdin, &msg).await.is_err() {
                                let _ = reply.send(Err(LspError::ServerGone));
                            } else {
                                self.pending.insert(id, reply);
                            }
                        }
                        Some(Cmd::Kill) => {
                            let _ = self.child.start_kill();
                            let _ = self.child.wait().await;
                            let _ = self.event_tx.send(LspEvent::Exited);
                            break;
                        }
                        None => break, // all handles dropped
                    }
                }

                frame = incoming_rx.recv() => {
                    match frame {
                        Some(bytes) => self.on_incoming(&bytes).await,
                        None => {
                            // stdout closed: clangd exited.
                            let _ = self.event_tx.send(LspEvent::Exited);
                            break;
                        }
                    }
                }
            }
        }
        // Fail every still-pending request so callers unblock promptly.
        for (_, reply) in self.pending.drain() {
            let _ = reply.send(Err(LspError::ServerGone));
        }
    }

    /// Dispatch one incoming message: response → waiter, server request →
    /// default answer, notification → event.
    async fn on_incoming(&mut self, bytes: &[u8]) {
        let Ok(msg) = serde_json::from_slice::<Value>(bytes) else {
            return;
        };

        let has_method = msg.get("method").is_some();
        let id = msg.get("id");

        match (id, has_method) {
            // Server-initiated request: id + method.
            (Some(id), true) => {
                self.answer_server_request(id.clone(), msg.get("method"), msg.get("params"))
                    .await;
            }
            // Response to one of our requests: id, no method.
            (Some(id), false) => {
                if let Some(id) = id.as_u64() {
                    if let Some(reply) = self.pending.remove(&id) {
                        let res = if let Some(err) = msg.get("error") {
                            Err(LspError::Protocol(err.to_string()))
                        } else {
                            Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = reply.send(res);
                    }
                }
            }
            // Notification: method, no id.
            (None, true) => self.on_notification(&msg),
            (None, false) => {}
        }
    }

    /// Answer a server→client request with a sensible default so clangd never
    /// stalls waiting on the client. clangd 17 issues `client/registerCapability`,
    /// `workspace/configuration`, and `window/workDoneProgress/create`; we also
    /// blanket-answer anything else with `null`.
    async fn answer_server_request(&mut self, id: Value, method: Option<&Value>, params: Option<&Value>) {
        let method = method.and_then(Value::as_str).unwrap_or("");
        let result = match method {
            // Return one settings object per requested item (clangd asks for
            // per-file config); empty objects = "use defaults".
            "workspace/configuration" => {
                let n = params
                    .and_then(|p| p.get("items"))
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                Value::Array(vec![json!({}); n])
            }
            // registerCapability / unregisterCapability / workDoneProgress/create
            // and every other server request: null is the correct/benign reply.
            _ => Value::Null,
        };
        let _ = write_message(
            &mut self.stdin,
            &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        )
        .await;
    }

    /// Forward the notifications jade cares about. Only `publishDiagnostics` is
    /// surfaced (inventory §4.4); `$/progress`, `window/logMessage`, etc. are
    /// intentionally dropped, mirroring the TS client which subscribed to
    /// diagnostics alone (`lsp-client.ts:209-215`).
    fn on_notification(&self, msg: &Value) {
        if msg.get("method").and_then(Value::as_str) != Some("textDocument/publishDiagnostics") {
            return;
        }
        let Some(params) = msg.get("params") else {
            return;
        };
        let Ok(parsed) =
            serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(params.clone())
        else {
            return;
        };
        let _ = self.event_tx.send(LspEvent::Diagnostics {
            path: path_from_uri(&parsed.uri),
            diagnostics: parsed.diagnostics,
        });
    }
}

/// Read side: reassembles LSP frames from a `ChildStdout`.
struct FramedReader {
    stdout: BufReader<ChildStdout>,
    decoder: FrameDecoder,
    chunk: Box<[u8; 8192]>,
}

impl FramedReader {
    fn new(stdout: ChildStdout) -> Self {
        Self {
            stdout: BufReader::new(stdout),
            decoder: FrameDecoder::new(),
            chunk: Box::new([0u8; 8192]),
        }
    }

    /// Next complete message body, `Ok(None)` on EOF.
    async fn next(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(msg) = self.decoder.next_message() {
                return Ok(Some(msg));
            }
            let n = self.stdout.read(self.chunk.as_mut_slice()).await?;
            if n == 0 {
                return Ok(None);
            }
            self.decoder.push(&self.chunk[..n]);
        }
    }
}

/// Frame and write a JSON value to clangd's stdin.
async fn write_message(stdin: &mut ChildStdin, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).expect("serialize outgoing JSON");
    stdin.write_all(&encode_message(&body)).await?;
    stdin.flush().await
}

/// Read `ServerCapabilities.textDocumentSync` and reduce it to a
/// [`TextDocumentSyncKind`]. Handles both the bare-kind and the
/// `TextDocumentSyncOptions.change` encodings; defaults to `FULL` when absent
/// (LSP's own default).
fn parse_sync_kind(init_result: &Value) -> TextDocumentSyncKind {
    let Some(caps) = init_result.get("capabilities") else {
        return TextDocumentSyncKind::FULL;
    };
    let Some(sync) = caps.get("textDocumentSync") else {
        return TextDocumentSyncKind::FULL;
    };
    match serde_json::from_value::<TextDocumentSyncCapability>(sync.clone()) {
        Ok(TextDocumentSyncCapability::Kind(k)) => k,
        Ok(TextDocumentSyncCapability::Options(o)) => o.change.unwrap_or(TextDocumentSyncKind::NONE),
        Err(_) => TextDocumentSyncKind::FULL,
    }
}

/// Flatten hover contents to a single string for `sizeof` scanning, covering
/// all three `HoverContents` encodings (scalar / array / markup).
fn flatten_hover(hover: &Hover) -> String {
    use lsp_types::{HoverContents, MarkedString};
    fn marked(m: &MarkedString) -> String {
        match m {
            MarkedString::String(s) => s.clone(),
            MarkedString::LanguageString(ls) => ls.value.clone(),
        }
    }
    match &hover.contents {
        HoverContents::Scalar(m) => marked(m),
        HoverContents::Array(ms) => ms.iter().map(marked).collect::<Vec<_>>().join("\n"),
        HoverContents::Markup(mc) => mc.value.clone(),
    }
}

/// Extract the byte size from a `sizeof = <n>` occurrence in hover text. Port of
/// the TS regex `/sizeof\s*=\s*(\d+)/` (`lsp-client.ts:293`), hand-rolled to
/// avoid a regex dependency. Returns the first match.
pub(crate) fn extract_sizeof(content: &str) -> Option<u64> {
    let bytes = content.as_bytes();
    let mut i = 0;
    while let Some(rel) = content[i..].find("sizeof") {
        let mut j = i + rel + "sizeof".len();
        // skip whitespace
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'=' {
            j += 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                if let Ok(n) = content[start..j].parse::<u64>() {
                    return Some(n);
                }
            }
        }
        i += rel + "sizeof".len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizeof_extraction_basic() {
        assert_eq!(extract_sizeof("struct Foo\nsize = 8, sizeof = 16"), Some(16));
        assert_eq!(extract_sizeof("sizeof=4"), Some(4));
        assert_eq!(extract_sizeof("sizeof   =   32 bytes"), Some(32));
    }

    #[test]
    fn sizeof_extraction_none() {
        assert_eq!(extract_sizeof("no size here"), None);
        assert_eq!(extract_sizeof("sizeof = abc"), None);
        assert_eq!(extract_sizeof("sizeof"), None);
    }

    #[test]
    fn sizeof_takes_first_match() {
        assert_eq!(extract_sizeof("sizeof = 1; sizeof = 2"), Some(1));
    }

    #[test]
    fn sync_kind_from_bare_number() {
        let v = json!({ "capabilities": { "textDocumentSync": 2 } });
        assert_eq!(parse_sync_kind(&v), TextDocumentSyncKind::INCREMENTAL);
    }

    #[test]
    fn sync_kind_from_options() {
        let v = json!({ "capabilities": { "textDocumentSync": { "openClose": true, "change": 1 } } });
        assert_eq!(parse_sync_kind(&v), TextDocumentSyncKind::FULL);
    }

    #[test]
    fn sync_kind_default_full_when_absent() {
        assert_eq!(parse_sync_kind(&json!({})), TextDocumentSyncKind::FULL);
    }
}
