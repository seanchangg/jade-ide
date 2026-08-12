# Jade Telemetry Protocol

This document is the **authoritative** wire specification for the Jade IDE
telemetry channel. The probe client (the injected GPU tensor-streaming dylib)
and any instrumented training program are built against this document.

## Transport

- **Socket**: Unix domain (stream) socket.
- **Address**: the IDE creates the socket and exports its path to every process
  it spawns via the environment variable:

  ```
  JADE_TELEMETRY_SOCK=<path>
  ```

  The path is `os.tmpdir() + "/jade-telemetry-<ide-pid>.sock"` but clients must
  read it from `JADE_TELEMETRY_SOCK` and must **not** hardcode it. If the
  variable is unset/empty, telemetry is unavailable — clients should degrade
  gracefully (fall back to the legacy `__JADE_*` stdout lines, see below).
- **Framing**: newline-delimited JSON (**NDJSON**), UTF-8. One complete JSON
  object per line, terminated by `\n`. No embedded newlines inside a message.
- **Direction**: bidirectional. Client→IDE carries discovery + data; IDE→Client
  carries control (`track`) messages.
- **Multiple clients**: allowed and concurrent (e.g. the program itself plus an
  injected probe dylib). Each connection is independent; the IDE replays the
  current selection to every newly connected client.
- **Robustness**: the IDE tolerates partial lines across TCP-style chunk
  boundaries, and silently skips any line that is not valid JSON or not a
  recognized message. Clients should do the same for `track` messages.

## Client → IDE messages

### `decl` — declare an item (optional but recommended)

Announces that a named scalar / timer / buffer exists. Sending `decl` lets the
item appear in the IDE's telemetry sidebar **before** any data flows. It is
optional: any name first seen via a data message (`scalar` / `timing` /
`tensor`) is auto-registered identically.

```json
{"type":"decl","kind":"scalar","name":"loss","meta":{"label":"train loss"}}
{"type":"decl","kind":"timer","name":"forward_ms"}
{"type":"decl","kind":"buffer","name":"grad.layer0","meta":{"rows":1024,"cols":1024,"dtype":"f32","label":"layer0 grad"}}
```

- `kind`: `"scalar"` | `"timer"` | `"buffer"`.
- `name`: string, unique per kind. Used as the registry key and localStorage
  key for the checkbox.
- `meta` (optional): `{ "rows":N, "cols":N, "dtype":"f32", "label":"..." }`.
  Primarily used for buffers (declares native tensor shape/dtype). All fields
  optional; late-arriving `meta` (e.g. shape learned from the first frame) is
  merged.
- `meta.renamedFrom` (optional, string): set when re-declaring an item that was
  previously declared under a different name — e.g. a Metal buffer first
  declared at allocation as `"buffer#3"` and later given the label
  `"model.weights"`. The IDE migrates the existing registry entry (carrying its
  enabled/checkbox state) to the new name instead of leaving a stale placeholder
  row. Example:

  ```json
  {"type":"decl","kind":"buffer","name":"model.weights","meta":{"renamedFrom":"buffer#3","bytes":262144,"storage":"private"}}
  ```

### `scalar` — a scalar sample

```json
{"type":"scalar","name":"loss","step":42,"value":0.1734,"t":1752300000.5}
```

- `step`: integer training step.
- `value`: the scalar value (number).
- `t`: unix time in **seconds** (float). Optional — the IDE substitutes its own
  receive time if omitted.

Scalars always flow to the renderer, but only **checked** scalars are plotted.
The first 3 scalars discovered are auto-checked so the panel isn't empty.

### `timing` — an operation timing sample

```json
{"type":"timing","name":"forward","ms":12.4,"step":42}
```

- `ms`: duration in milliseconds (number).
- `step`: integer training step.

Timings always flow to the renderer; only **checked** timers are plotted in the
timing breakdown. Timers are **not** auto-checked (opt-in).

### `tensor` — a GPU-buffer frame

```json
{"type":"tensor","name":"grad.layer0","step":42,"rows":128,"cols":128,"dtype":"f32","data":"<base64>"}
```

- `rows`, `cols`: dimensions of **this frame** (already downsampled).
- `dtype`: currently `"f32"`.
- `data`: base64 of a **row-major** `float32` array of exactly `rows*cols`
  elements (little-endian, as produced by a native `float[]`).
- **Downsampling is the client's responsibility.** The client MUST downsample
  to at most `maxDim × maxDim` (see `track.maxDim`, default 128) before
  encoding. The IDE renders frames as-is.
- **Gating**: the client MUST only send `tensor` frames for buffers it has been
  told to stream via a `track` message with `enabled:true`. The IDE also drops
  frames for un-enabled buffers defensively.

## IDE → Client messages

### `track` — start/stop streaming an item

```json
{"type":"track","kind":"buffer","name":"grad.layer0","enabled":true,"maxDim":128}
```

Sent by the IDE when:

1. the user toggles the item's checkbox in the telemetry sidebar, **and**
2. immediately after a client connects — the IDE replays a `track` with
   `enabled:true` for **every currently-enabled item** (so a late-joining probe
   learns what to stream).

- `enabled`: whether the IDE wants data for this item.
  - For `buffer`: the client should start (`true`) or stop (`false`) streaming
    `tensor` frames for `name`.
  - For `scalar`/`timer`: informational (the client may keep sending; the IDE
    filters plotting itself). Clients may use it to avoid unnecessary work.
- `maxDim`: max rows/cols the client should downsample buffers to before
  sending (default 128).

Note: the IDE only ever replays `enabled:true` tracks on connect. A client that
never receives a `track` for a buffer should assume `enabled:false`.

## Legacy stdout back-compat

The prior magic-stdout protocol still works and feeds the **same** registry — a
name first seen this way is auto-registered exactly like a socket `decl`:

```
__JADE_SCALAR|<name>|<step>|<value>|<timestamp>
__JADE_TIMING|<name>|<ms>|<step>
```

`build-runner.ts` parses these from the program's stdout and routes them through
the telemetry registry (`TelemetryServer.ingestScalar` / `ingestTiming`), so
legacy programs appear in the sidebar and are auto-checked identically.

## Renderer notes (for downstream consumers)

- Decoded tensor frames are published on the renderer store as the
  **`tensorFrame`** event: `{ name, step, rows, cols, dtype, data: Float32Array }`.
  `training-view.ts` keeps the latest **32 frames per tracked buffer** in a ring
  buffer and renders the newest as a diverging blue-white-red heatmap centered
  at 0. A future 3D visualization can subscribe to `store.on('tensorFrame', …)`
  to consume the same frames without re-decoding.

## Message summary

| Direction     | type      | required fields                                   |
| ------------- | --------- | ------------------------------------------------- |
| Client → IDE  | `decl`    | `kind`, `name` (+ optional `meta`)                |
| Client → IDE  | `scalar`  | `name`, `step`, `value` (+ optional `t`)          |
| Client → IDE  | `timing`  | `name`, `ms`, `step`                              |
| Client → IDE  | `tensor`  | `name`, `step`, `rows`, `cols`, `dtype`, `data`   |
| IDE → Client  | `track`   | `kind`, `name`, `enabled`, `maxDim`               |
