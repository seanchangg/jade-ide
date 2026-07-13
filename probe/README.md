# forge-probe: zero-instrumentation Metal telemetry

**Question investigated:** can forge-ide read GPU VRAM buffers (including
private, never-copied-to-CPU intermediate buffers) from a running training
process, without the user adding `__FORGE_*` macros to their code?

**Answer: yes — proven working on this machine (Apple M4, macOS 25.3).**
`make && python3 mock_server.py` reproduces the end-to-end test (PASS).

## How it works

`forge_probe.dylib` is injected into the user's program at launch via
`DYLD_INSERT_LIBRARIES` (the IDE controls process launch in
`build-runner.ts`, so this is one env var). It needs no code changes, no
recompilation, and no linking in the user's project:

1. **Device capture** — a `__DATA,__interpose` section interposes
   `MTLCreateSystemDefaultDevice`. When the app gets its device, the probe
   swizzles the concrete (private) device class via the Objective-C runtime.
2. **Buffer discovery** — swizzled `newBufferWithLength:options:` /
   `newBufferWithBytes:length:options:` register every allocation (size,
   storage mode). A swizzled `setLabel:` picks up the human-readable names
   (`model.weights` etc.) apps assign after creation. Each discovery emits a
   `decl` message so the IDE sidebar can list it with a checkbox.
3. **Automatic GPU timing** — swizzled `commit` on the command-buffer class
   attaches a completion handler reading `GPUStartTime`/`GPUEndTime`, giving
   per-command-buffer GPU timings labeled by `commandBuffer.label`. This
   replaces manual `__FORGE_TIMING` for GPU work entirely.
4. **VRAM readback** — for `MTLStorageModeShared`/`Managed` buffers, contents
   are read directly. For `MTLStorageModePrivate` (VRAM-only) buffers, the
   probe blits to a shared staging buffer on its own command queue, scheduled
   from the app's command-buffer *completion* handler so it never races
   in-flight GPU work. Verified byte-accurate (weights evolve step to step).
5. **Selection control** — the probe connects to the unix socket in
   `$FORGE_TELEMETRY_SOCK` (NDJSON, see `../docs/telemetry-protocol.md`) and
   honors IDE `{"type":"track",...}` messages: only checked buffers stream,
   downsampled (average-pooled) to `maxDim×maxDim` before base64 encoding.
   Fallback without a socket: `FORGEJSON|`-prefixed lines on stderr;
   `FORGE_TRACK_ALL=1` streams everything.

## Files

- `forge_probe.mm` — the probe dylib
- `test_train.mm` — deliberately uninstrumented Metal SGD loop (private
  weights buffer, shared gradients) used to validate the probe
- `mock_server.py` — stand-in for the IDE telemetry server; asserts the
  protocol end-to-end
- `Makefile` — `make` builds both; `make clean`

## Limitations & notes

- **Shape is unknowable from a raw `MTLBuffer`** — it's just bytes. The probe
  currently guesses the largest square of float32s. The IDE should let the
  user set rows×cols per buffer (a future `track` field, e.g. `"shape":[R,C]`)
  or infer from kernel argument metadata.
- **dtype assumed f32.** f16/bf16 support = read the same bytes, convert in
  the probe; needs a shape/dtype hint for the same reason as above.
- **Hardened-runtime binaries block `DYLD_INSERT_LIBRARIES`.** Irrelevant for
  local dev builds (ad-hoc signed, no hardened runtime) — which is everything
  forge-ide compiles itself. System Python + hardened frameworks would need
  the `com.apple.security.cs.allow-dyld-environment-variables` entitlement.
- **Overhead** is bounded by design: tensor readback runs every 4th command
  buffer, only for tracked buffers, off the app's queue. Blit of a 256×256
  f32 buffer measured well under 1 ms end-to-end here. Timing capture is one
  completion handler per command buffer (~µs).
- Works for anything that allocates through the Metal device object —
  hand-written kernels, MPS, and MLX included.

## IDE integration (next step)

In `build-runner.ts`, alongside the existing `FORGE_TELEMETRY_SOCK` env:

```ts
env.DYLD_INSERT_LIBRARIES = path.join(app.getAppPath(), 'probe', 'forge_probe.dylib');
```

Then the telemetry sidebar's buffer checkboxes drive `track` messages, and
tensor frames flow to the training view / future 3D weight-grid renderer.
