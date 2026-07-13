# Performance findings — issues in files owned by other agents

These are real performance problems found during the perf audit that live in
files this agent is not allowed to modify. Each entry lists `file:line`, the
problem, and the recommended fix.

## 1. build-runner.ts — one IPC message per allocation/free event (firehose)

- **Location:** `src/main/build-runner.ts:58` (inside `parseForgeOutput`),
  reached from the stdout line loop at `src/main/build-runner.ts:425-430`.
- **Problem:** Every `__FORGE_ALLOC` / `__FORGE_FREE` line the instrumented
  program prints results in a separate `win.webContents.send(IPC.BUILD_MEMORY_EVENT, event)`
  call. During a training run these can arrive thousands per second. Each IPC
  message crosses the main→renderer boundary and wakes the renderer's
  `onMemoryEvent` handler individually. This is the root "firehose" — the
  renderer side has now been made resilient (events are processed cheaply and
  the memory-bar store write is coalesced to once per animation frame — see
  `src/renderer/editor/memory-decorations.ts`), but the per-event IPC traffic
  itself remains a cost that only the main process can remove.
- **Recommended fix:** Batch allocation/free events in `run()`. Accumulate
  parsed `AllocationEvent`s into an array and flush them as a single
  `IPC.BUILD_MEMORY_EVENT` message carrying `{ type: 'alloc-batch', events: [...] }`
  on a timer (e.g. every 50–100ms) or when the batch reaches a cap (e.g. 500
  events). Update the renderer's `onMemoryEvent` handler to accept the batch
  form and loop over `events` (the per-event processing there is already O(1)).
  This collapses thousands of IPC round-trips into ~10–20/sec.

## 2. build-runner.ts — debug `console.log` on the heap-summary path

- **Location:** `src/main/build-runner.ts:176` and `:187` (`parseHeapSummary`).
- **Problem:** `console.log` calls on the heap-summary path. This fires only
  once per run (at program exit), so impact is low, but the matching
  per-event `console.log` in the renderer (`memory-decorations.ts`) has been
  removed as part of this pass. Recommend removing these two as well for
  consistency and to avoid noise if the summary format ever becomes periodic.
- **Recommended fix:** Delete the two `console.log` statements.

## Notes

- `src/renderer/panels/memory-overlay.ts` is dead code: it is not imported or
  mounted anywhere (only its CSS classes exist in `main.css`). Its
  `store.on(...)` subscriptions never register, so it currently has zero
  runtime cost. If it is ever wired up, apply the same coalescing/element-reuse
  patterns used in `runtime-panel.ts`.
