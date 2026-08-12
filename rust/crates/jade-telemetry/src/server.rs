//! The telemetry server: accepts Unix-socket clients, maintains the item
//! registry, gates/forwards data to the UI, and broadcasts `track` control
//! messages. Port of `src/main/telemetry-server.ts`.
//!
//! Semantics preserved from the TS implementation:
//! - stale socket file deleted on start, socket file removed on stop
//! - multiple concurrent clients; enabled `track`s replayed to new clients
//! - registry defaults `enabled: false`, `maxDim: 128`; late `meta` merged and
//!   the decl re-emitted
//! - `meta.renamedFrom` migrates an existing entry, carrying enabled state
//! - tensor frames for disabled buffers are dropped
//! - `track` messages sent to clients use the client's own (probe) name for
//!   aliased buffers
//!
//! Buffer alias resolution via `atos` symbolication (`resolveBufferAlias`) is
//! wired through the [`Symbolicator`] hook: `jade-build` installs an
//! implementation (atos + source-line variable-name extraction) via
//! [`TelemetryServer::set_symbolicator`]; this module owns the alias maps, the
//! `aliasPending` re-entrancy guard, the collision-resolution / rename step, and
//! the `knownAlias` re-declaration short-circuit (telemetry-server.ts:186-289).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::protocol::*;

/// Symbolication hook: given allocation-site return addresses, the target
/// executable, and its ASLR slide (`load` address), return candidate variable
/// names for the buffer's allocation site, **innermost frame first**.
///
/// This is the `atos` + source-line-regex half of `telemetry-server.ts`'s
/// `resolveBufferAlias` / `extractVarName` (`:239-319`). It is toolchain
/// business, so `jade-build` provides the implementation; the server keeps the
/// alias bookkeeping (collision resolution, `probeOf`/`aliasOf` maps, rename).
///
/// `exe` is `None` when the probe did not report `meta.exe`; the implementation
/// is expected to fall back to the executable it was configured with in
/// `run()`.
pub trait Symbolicator: Send + Sync {
    fn variable_names(&self, addrs: &[String], exe: Option<&str>, load: &str) -> Vec<String>;

    /// Resolve several allocation sites sharing one `(exe, load)` in a single
    /// pass. A probed app declares every buffer in one startup burst — hundreds
    /// of decls — and one `atos` process per decl thundering-herds the machine
    /// until invocations blow their timeout and those buffers keep their
    /// fallback names. Implementations that shell out should override this to
    /// spawn ONE process for the whole batch; the default just loops.
    fn variable_names_batch(
        &self,
        addr_sets: &[Vec<String>],
        exe: Option<&str>,
        load: &str,
    ) -> Vec<Vec<String>> {
        addr_sets
            .iter()
            .map(|addrs| self.variable_names(addrs, exe, load))
            .collect()
    }
}

/// Events surfaced to the UI layer (the Rust analogue of the renderer IPC
/// channels `telemetry:decl` / `telemetry:tensor` / the legacy scalar/timing
/// build channels).
#[derive(Debug, Clone)]
pub enum Event {
    Decl {
        kind: Kind,
        name: String,
        meta: Option<Map<String, Value>>,
        renamed_from: Option<String>,
    },
    Scalar(Scalar),
    Timing(Timing),
    /// Frame for an enabled buffer, base64 already decoded (decode-once, as the
    /// TS renderer did in telemetry-panel.ts).
    Tensor {
        name: String,
        step: i64,
        rows: u32,
        cols: u32,
        src_rows: Option<u32>,
        src_cols: Option<u32>,
        dtype: String,
        data: Vec<f32>,
    },
}

#[derive(Debug, Clone)]
struct Item {
    enabled: bool,
    max_dim: u32,
    meta: Option<Map<String, Value>>,
    shape_rows: Option<u32>,
    shape_cols: Option<u32>,
}

impl Default for Item {
    fn default() -> Self {
        Item {
            enabled: false,
            max_dim: DEFAULT_MAX_DIM,
            meta: None,
            shape_rows: None,
            shape_cols: None,
        }
    }
}

fn key_of(kind: Kind, name: &str) -> String {
    format!("{} {}", kind.as_str(), name)
}

struct State {
    items: HashMap<String, Item>,
    /// display key -> probe (client-side) name, for aliased buffers
    probe_names: HashMap<String, String>,
    /// probe key -> display name
    display_names: HashMap<String, String>,
    clients: HashMap<u64, mpsc::UnboundedSender<String>>,
    events: mpsc::UnboundedSender<Event>,
    /// Probe names whose atos symbolication is in flight (telemetry-server.ts
    /// `aliasPending`), so a re-declare mid-resolve doesn't spawn a duplicate.
    alias_pending: HashSet<String>,
    /// Installed by `jade-build` before a run; see [`Symbolicator`].
    symbolicator: Option<Arc<dyn Symbolicator>>,
    /// Queue feeding the single symbolication worker (spawned on first use).
    sym_tx: Option<mpsc::UnboundedSender<SymJob>>,
}

/// One buffer decl awaiting variable-name resolution.
struct SymJob {
    probe_name: String,
    addrs: Vec<String>,
    exe: Option<String>,
    load: String,
}

impl State {
    fn display_name(&self, kind: Kind, probe_name: &str) -> String {
        self.display_names
            .get(&key_of(kind, probe_name))
            .cloned()
            .unwrap_or_else(|| probe_name.to_string())
    }

    fn probe_name(&self, kind: Kind, display_name: &str) -> String {
        self.probe_names
            .get(&key_of(kind, display_name))
            .cloned()
            .unwrap_or_else(|| display_name.to_string())
    }

    /// Register `name` if new (emitting a decl), or merge late-arriving meta
    /// into an existing entry and re-emit the decl — matching TS `declare()`.
    fn declare(&mut self, kind: Kind, name: &str, meta: Option<&Map<String, Value>>) {
        let key = key_of(kind, name);
        match self.items.get_mut(&key) {
            None => {
                self.items.insert(
                    key,
                    Item {
                        meta: meta.cloned(),
                        ..Item::default()
                    },
                );
                let _ = self.events.send(Event::Decl {
                    kind,
                    name: name.to_string(),
                    meta: meta.cloned(),
                    renamed_from: None,
                });
            }
            Some(item) => {
                if let Some(new_meta) = meta {
                    let merged_meta = match item.meta.take() {
                        Some(mut existing) => {
                            for (k, v) in new_meta {
                                existing.insert(k.clone(), v.clone());
                            }
                            existing
                        }
                        None => new_meta.clone(),
                    };
                    item.meta = Some(merged_meta.clone());
                    let _ = self.events.send(Event::Decl {
                        kind,
                        name: name.to_string(),
                        meta: Some(merged_meta),
                        renamed_from: None,
                    });
                }
            }
        }
    }

    /// Migrate an item to a new name, preserving enabled state. If the
    /// destination already exists, the old placeholder is simply dropped.
    /// Pure registry migration — alias maps are managed by the callers, since
    /// probe-initiated and server-initiated renames differ (see handle_line
    /// and TelemetryServer::rename).
    fn rename(&mut self, kind: Kind, from: &str, to: &str, meta: Option<&Map<String, Value>>) {
        let from_key = key_of(kind, from);
        let to_key = key_of(kind, to);
        if let Some(mut item) = self.items.remove(&from_key) {
            if !self.items.contains_key(&to_key) {
                if let Some(new_meta) = meta {
                    let mut merged = item.meta.take().unwrap_or_default();
                    for (k, v) in new_meta {
                        merged.insert(k.clone(), v.clone());
                    }
                    item.meta = Some(merged);
                }
                self.items.insert(to_key.clone(), item);
            }
        } else if !self.items.contains_key(&to_key) {
            self.items.insert(
                to_key,
                Item {
                    meta: meta.cloned(),
                    ..Item::default()
                },
            );
        }
        let _ = self.events.send(Event::Decl {
            kind,
            name: to.to_string(),
            meta: meta.cloned(),
            renamed_from: Some(from.to_string()),
        });
    }

    /// Turn symbolicated variable-name parts (innermost first) into a unique
    /// buffer alias and migrate the probe's entry onto it, recording the
    /// two-way alias so `track` messages translate back to the probe's name.
    ///
    /// The alias is always the FULL recovered scope chain ("blocks.ln2.buf"),
    /// with `#2`, `#3`… suffixes only for identical chains (the same alloc
    /// site run once per layer). The TS original (telemetry-server.ts:267-289)
    /// kept the *shortest* unique name instead, which handed the first-resolved
    /// buffer the bare innermost name ("xBuffer") and qualified only later
    /// arrivals — and since atos resolutions complete in nondeterministic
    /// order, which buffer won the generic name changed run to run. Full
    /// qualification keeps sibling instances consistently specific. Always
    /// clears the `aliasPending` guard (the TS `finally`).
    fn apply_symbolicated_alias(&mut self, kind: Kind, probe_name: &str, parts: Vec<String>) {
        if !parts.is_empty() {
            let mut alias = parts[0].clone();
            for outer in &parts[1..] {
                alias = format!("{}.{}", outer, alias);
            }
            if self.probe_names.contains_key(&key_of(kind, &alias)) {
                let mut k = 2;
                while self
                    .probe_names
                    .contains_key(&key_of(kind, &format!("{}#{}", alias, k)))
                {
                    k += 1;
                }
                alias = format!("{}#{}", alias, k);
            }
            if alias != probe_name {
                self.probe_names
                    .insert(key_of(kind, &alias), probe_name.to_string());
                self.display_names
                    .insert(key_of(kind, probe_name), alias.clone());
                self.rename(kind, probe_name, &alias, None);
            }
        }
        self.alias_pending.remove(probe_name);
    }

    fn track_line(&self, kind: Kind, display_name: &str, item: &Item) -> String {
        let track = Track {
            msg_type: "track",
            kind,
            name: self.probe_name(kind, display_name),
            enabled: item.enabled,
            max_dim: item.max_dim,
            rows: item.shape_rows,
            cols: item.shape_cols,
        };
        let mut line = serde_json::to_string(&track).expect("track serializes");
        line.push('\n');
        line
    }

    fn broadcast(&mut self, line: &str) {
        self.clients
            .retain(|_, tx| tx.send(line.to_string()).is_ok());
    }
}

/// A running telemetry server. Dropping the handle does not stop the server;
/// call [`TelemetryServer::stop`].
pub struct TelemetryServer {
    socket_path: PathBuf,
    state: Arc<Mutex<State>>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl TelemetryServer {
    /// Default PID-scoped socket path, matching the TS server:
    /// `<tmpdir>/jade-telemetry-<pid>.sock`.
    pub fn default_socket_path() -> PathBuf {
        std::env::temp_dir().join(format!("jade-telemetry-{}.sock", std::process::id()))
    }

    /// Bind and start accepting clients. Returns the server handle and the
    /// event stream for the UI layer.
    pub fn start(socket_path: PathBuf) -> std::io::Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        // Crash recovery: remove a stale socket file from a previous run.
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)?;

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(State {
            items: HashMap::new(),
            probe_names: HashMap::new(),
            display_names: HashMap::new(),
            clients: HashMap::new(),
            events: events_tx,
            alias_pending: HashSet::new(),
            symbolicator: None,
            sym_tx: None,
        }));

        let accept_state = state.clone();
        let accept_task = tokio::spawn(async move {
            let mut next_client_id: u64 = 0;
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        next_client_id += 1;
                        tokio::spawn(handle_client(stream, next_client_id, accept_state.clone()));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok((
            TelemetryServer {
                socket_path,
                state,
                accept_task,
            },
            events_rx,
        ))
    }

    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Install the `atos` symbolication hook. `jade-build` calls this before a
    /// run so probe-discovered buffers get their allocation-site variable names
    /// (telemetry-server.ts:196-205). Replaces any previously-installed hook.
    pub fn set_symbolicator(&self, symbolicator: Arc<dyn Symbolicator>) {
        self.state.lock().unwrap().symbolicator = Some(symbolicator);
    }

    /// UI → server: enable/disable an item and broadcast the `track` to every
    /// connected client. Port of TS `setTrack`.
    pub fn set_track(
        &self,
        kind: Kind,
        name: &str,
        enabled: bool,
        max_dim: Option<u32>,
        shape: Option<(u32, u32)>,
    ) {
        let mut state = self.state.lock().unwrap();
        state.declare(kind, name, None);
        let key = key_of(kind, name);
        let line = {
            let item = state.items.get_mut(&key).expect("declared above");
            item.enabled = enabled;
            item.max_dim = match max_dim {
                Some(d) if d > 0 => d,
                _ => DEFAULT_MAX_DIM,
            };
            if let Some((rows, cols)) = shape {
                if rows > 0 && cols > 0 {
                    item.shape_rows = Some(rows);
                    item.shape_cols = Some(cols);
                }
            }
            let item = item.clone();
            state.track_line(kind, name, &item)
        };
        state.broadcast(&line);
    }

    /// Server-initiated rename (e.g. after symbolicating a buffer's allocation
    /// site with atos). The client still speaks its own name, so the alias is
    /// recorded in both directions and `track` messages are translated back.
    pub fn rename(&self, kind: Kind, from: &str, to: &str) {
        let mut state = self.state.lock().unwrap();
        state
            .probe_names
            .insert(key_of(kind, to), from.to_string());
        state
            .display_names
            .insert(key_of(kind, from), to.to_string());
        state.rename(kind, from, to, None);
    }

    /// Legacy `__JADE_SCALAR` stdout path: auto-registers like a socket decl
    /// and forwards to the UI. Port of TS `ingestScalar`.
    pub fn ingest_scalar(&self, mut scalar: Scalar) {
        if scalar.t.is_none() {
            scalar.t = Some(now_unix_seconds());
        }
        let mut state = self.state.lock().unwrap();
        state.declare(Kind::Scalar, &scalar.name.clone(), None);
        let _ = state.events.send(Event::Scalar(scalar));
    }

    /// Legacy `__JADE_TIMING` stdout path. Port of TS `ingestTiming`.
    pub fn ingest_timing(&self, timing: Timing) {
        let mut state = self.state.lock().unwrap();
        state.declare(Kind::Timer, &timing.name.clone(), None);
        let _ = state.events.send(Event::Timing(timing));
    }

    /// Whether an item is currently enabled (used by tests and the UI).
    pub fn is_enabled(&self, kind: Kind, name: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .items
            .get(&key_of(kind, name))
            .map(|i| i.enabled)
            .unwrap_or(false)
    }

    /// Stop accepting, disconnect clients, remove the socket file.
    pub fn stop(self) {
        self.accept_task.abort();
        let mut state = self.state.lock().unwrap();
        state.clients.clear(); // writer tasks end when their channels close
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn now_unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

async fn handle_client(stream: UnixStream, client_id: u64, state: Arc<Mutex<State>>) {
    let (mut reader, mut writer) = stream.into_split();

    // Writer half: control messages queued for this client.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // Replay current selections so a late-joining probe learns what to stream.
    // Per the spec, only enabled:true tracks are replayed on connect.
    {
        let mut state = state.lock().unwrap();
        let replays: Vec<String> = state
            .items
            .iter()
            .filter(|(_, item)| item.enabled)
            .map(|(key, item)| {
                let (kind_str, name) = key.split_once(' ').expect("key format");
                let kind = match kind_str {
                    "scalar" => Kind::Scalar,
                    "timer" => Kind::Timer,
                    _ => Kind::Buffer,
                };
                state.track_line(kind, name, item)
            })
            .collect();
        for line in replays {
            let _ = tx.send(line);
        }
        state.clients.insert(client_id, tx);
    }

    let writer_task = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Reader half: NDJSON framing with partial-line reassembly and the 8 MB
    // runaway-line guard.
    let mut buffer = String::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));
                while let Some(newline) = buffer.find('\n') {
                    let line: String = buffer.drain(..=newline).collect();
                    handle_line(line.trim(), &state);
                }
                if buffer.len() > MAX_PARTIAL_LINE {
                    buffer.clear();
                }
            }
        }
    }

    state.lock().unwrap().clients.remove(&client_id);
    writer_task.abort();
}

fn handle_line(line: &str, state_arc: &Arc<Mutex<State>>) {
    if line.is_empty() {
        return;
    }
    let Some(message) = ClientMessage::parse(line) else {
        return; // malformed or unrecognized: skip silently
    };
    let mut state = state_arc.lock().unwrap();
    match message {
        ClientMessage::Decl(decl) => {
            let renamed_from = decl
                .meta
                .as_ref()
                .and_then(|m| m.get("renamedFrom"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            match renamed_from {
                // Probe-initiated rename: the client now speaks the NEW name,
                // so any server-side alias for the old name is dropped and no
                // reverse translation is recorded (telemetry-server.ts:169-184).
                Some(from) if from != decl.name => {
                    let old_alias = state.display_names.remove(&key_of(decl.kind, &from));
                    if let Some(alias) = &old_alias {
                        state.probe_names.remove(&key_of(decl.kind, alias));
                    }
                    let from_name = old_alias.unwrap_or(from);
                    state.rename(decl.kind, &from_name, &decl.name, decl.meta.as_ref());
                    // The client keys its own gating by its CURRENT name: a
                    // buffer enabled under the placeholder stops streaming
                    // after setLabel unless we re-broadcast the track under
                    // the new name. The Electron renderer only re-pushed when
                    // a stored pref applied (telemetry-panel.ts:154-158) —
                    // enabled-before-rename silently went dark there; fixed
                    // deliberately here (see jade-feature-inventory.md §11).
                    let key = key_of(decl.kind, &decl.name);
                    if let Some(item) = state.items.get(&key).cloned() {
                        if item.enabled {
                            let line = state.track_line(decl.kind, &decl.name, &item);
                            state.broadcast(&line);
                        }
                    }
                }
                _ => {
                    // A buffer re-declared on a later run (deterministic probe
                    // name) that we already aliased: declare under the alias so
                    // we don't resurrect the row the rename removed
                    // (telemetry-server.ts:186-194).
                    if decl.kind == Kind::Buffer {
                        if let Some(alias) = state
                            .display_names
                            .get(&key_of(Kind::Buffer, &decl.name))
                            .cloned()
                        {
                            state.declare(Kind::Buffer, &alias, decl.meta.as_ref());
                            return;
                        }
                    }
                    state.declare(decl.kind, &decl.name, decl.meta.as_ref());
                    maybe_symbolicate(&mut state, state_arc, &decl);
                }
            }
        }
        ClientMessage::Scalar(mut scalar) => {
            if scalar.t.is_none() {
                scalar.t = Some(now_unix_seconds());
            }
            scalar.name = state.display_name(Kind::Scalar, &scalar.name);
            state.declare(Kind::Scalar, &scalar.name.clone(), None);
            let _ = state.events.send(Event::Scalar(scalar));
        }
        ClientMessage::Timing(mut timing) => {
            timing.name = state.display_name(Kind::Timer, &timing.name);
            state.declare(Kind::Timer, &timing.name.clone(), None);
            let _ = state.events.send(Event::Timing(timing));
        }
        ClientMessage::Tensor(tensor) => {
            let display = state.display_name(Kind::Buffer, &tensor.name);
            let mut meta = Map::new();
            meta.insert("rows".into(), tensor.rows.into());
            meta.insert("cols".into(), tensor.cols.into());
            meta.insert("dtype".into(), tensor.dtype.clone().into());
            state.declare(Kind::Buffer, &display, Some(&meta));
            // Gate: drop frames for buffers the UI hasn't enabled, even if the
            // client sends them anyway.
            let enabled = state
                .items
                .get(&key_of(Kind::Buffer, &display))
                .map(|i| i.enabled)
                .unwrap_or(false);
            if !enabled {
                return;
            }
            let Some(data) = tensor.decode() else {
                return; // bad payload: skip silently like any malformed line
            };
            let _ = state.events.send(Event::Tensor {
                name: display,
                step: tensor.step,
                rows: tensor.rows,
                cols: tensor.cols,
                src_rows: tensor.src_rows,
                src_cols: tensor.src_cols,
                dtype: tensor.dtype,
                data,
            });
        }
    }
}

/// Kick off async variable-name resolution for a probe-discovered buffer whose
/// meta carries allocation-site addresses (telemetry-server.ts:196-205). The
/// blocking `atos`/source work runs off the runtime; the result is applied back
/// under the state lock. No-op unless a [`Symbolicator`] is installed and the
/// meta has an `addrs` array + a `load` address (atos needs `-l`).
fn maybe_symbolicate(state: &mut State, state_arc: &Arc<Mutex<State>>, decl: &Decl) {
    if decl.kind != Kind::Buffer {
        return;
    }
    if state.symbolicator.is_none() {
        return;
    }
    let Some(meta) = decl.meta.as_ref() else {
        return;
    };
    let addrs: Vec<String> = match meta.get("addrs").and_then(Value::as_array) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None => return,
    };
    if addrs.is_empty() {
        return;
    }
    // atos requires the ASLR load address; the exe may be supplied by the probe
    // or fall back to the run()-configured executable inside the symbolicator.
    let Some(load) = meta.get("load").and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    let exe = meta.get("exe").and_then(Value::as_str).map(str::to_string);

    let probe_name = decl.name.clone();
    if !state.alias_pending.insert(probe_name.clone()) {
        return; // already resolving this probe name
    }

    // One worker drains the queue in batches: a probed app declares every
    // buffer in one startup burst, and one atos process per decl (the previous
    // shape) ran hundreds of atos concurrently — enough contention that some
    // blew their timeout and kept their fallback names.
    let tx = match &state.sym_tx {
        Some(tx) => tx.clone(),
        None => {
            let (tx, rx) = mpsc::unbounded_channel();
            state.sym_tx = Some(tx.clone());
            spawn_sym_worker(rx, state_arc.clone());
            tx
        }
    };
    let _ = tx.send(SymJob {
        probe_name,
        addrs,
        exe,
        load,
    });
}

/// Symbolication worker: debounce-collect queued jobs, group them by
/// `(exe, load)`, resolve each group with ONE batched symbolicator call, and
/// apply the aliases. `alias_pending` entries are cleared by
/// `apply_symbolicated_alias` even when resolution yields nothing.
fn spawn_sym_worker(mut rx: mpsc::UnboundedReceiver<SymJob>, state_arc: Arc<Mutex<State>>) {
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut jobs = vec![first];
            // Collect the rest of the burst; cap the batch so one atos argv
            // stays reasonable.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(50);
            while jobs.len() < 128 {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(job)) => jobs.push(job),
                    _ => break,
                }
            }

            let mut groups: HashMap<(Option<String>, String), Vec<SymJob>> = HashMap::new();
            for job in jobs {
                groups
                    .entry((job.exe.clone(), job.load.clone()))
                    .or_default()
                    .push(job);
            }
            for ((exe, load), group) in groups {
                let Some(symbolicator) = state_arc.lock().unwrap().symbolicator.clone() else {
                    let mut state = state_arc.lock().unwrap();
                    for job in &group {
                        state.alias_pending.remove(&job.probe_name);
                    }
                    continue;
                };
                let addr_sets: Vec<Vec<String>> = group.iter().map(|j| j.addrs.clone()).collect();
                let names = tokio::task::spawn_blocking(move || {
                    symbolicator.variable_names_batch(&addr_sets, exe.as_deref(), &load)
                })
                .await
                .unwrap_or_default();
                let mut state = state_arc.lock().unwrap();
                for (i, job) in group.iter().enumerate() {
                    let parts = names.get(i).cloned().unwrap_or_default();
                    state.apply_symbolicated_alias(Kind::Buffer, &job.probe_name, parts);
                }
            }
        }
    });
}
