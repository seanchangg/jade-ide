import '../styles/telemetry.css';
import { store } from '../state';
import type {
  TelemetryDecl,
  TelemetryKind,
  TelemetryMeta,
  TelemetryTensor,
  TrainingScalar,
  TrainingTimingEvent,
} from '../../shared/types';

const forge = (window as any).forge;

// ─────────────────────────────────────────────────────────────────────────────
// Renderer-side telemetry registry + sidebar panel.
//
// This module is the single owner of the renderer's telemetry state:
//   • the discovered-item registry (source of truth for the selection UI)
//   • localStorage persistence of checkbox state (keyed by item name)
//   • the "TELEMETRY" sidebar panel (Scalars / Timers / Buffers)
//   • the single subscription point for telemetry IPC (decl + tensor) and the
//     legacy training-scalar/timing streams, so tensors are decoded exactly once
//
// Selection flow (checkbox → socket track message):
//   checkbox.change → registry.setEnabled(kind, name, enabled)
//     → localStorage write  (persist)
//     → forge.telemetry.setTrack(...)  (IPC → main → net.Socket clients)
//     → emitStructure()  (panel re-render + training-view re-plot)
//
// training-view.ts imports `telemetryRegistry` to plot only enabled scalars /
// timers, and subscribes to the store 'tensorFrame' event (published here) for
// decoded GPU-buffer frames.
// ─────────────────────────────────────────────────────────────────────────────

const LS_PREFIX = 'forge.telemetry.enabled.';
const LS_SHAPE_PREFIX = 'forge.telemetry.shape.';
const LS_MAXDIM_PREFIX = 'forge.telemetry.maxdim.';
// Default per-dimension streaming cap; lowerable per buffer (probe clamps to [4, 256]).
const DEFAULT_MAX_DIM = 256;
const AUTO_CHECK_SCALARS = 3;

// Decoded tensor frame, published on the store as 'tensorFrame'.
export interface TensorFrame {
  name: string;
  step: number;
  rows: number;
  cols: number;
  // Pre-pooling source dimensions (for axis labeling); may equal rows/cols.
  srcRows?: number;
  srcCols?: number;
  dtype: string;
  data: Float32Array; // row-major
}

export interface TelemetryRegistryItem {
  kind: TelemetryKind;
  name: string;
  enabled: boolean;
  meta?: TelemetryMeta;
  // User-provided tensor shape hint (buffers only), persisted per name.
  shapeRows?: number;
  shapeCols?: number;
  // Per-buffer streaming resolution override (max cells per dimension).
  maxDim?: number;
  // Live "last value" for the panel readout.
  lastValue?: number; // scalar: latest value; timer: latest ms
  lastStep?: number;
  lastRows?: number;
  lastCols?: number;
}

function keyOf(kind: TelemetryKind, name: string): string {
  return `${kind} ${name}`;
}

class TelemetryRegistry {
  readonly items = new Map<string, TelemetryRegistryItem>();
  private scalarDiscoveryOrder: string[] = [];
  private structureListeners = new Set<() => void>();
  private valueListeners = new Set<(key: string) => void>();

  // Fired when the item set / metadata / enabled-state changes (low frequency).
  onStructure(fn: () => void): () => void {
    this.structureListeners.add(fn);
    return () => this.structureListeners.delete(fn);
  }
  // Fired when an item's live last-value changes (high frequency).
  onValue(fn: (key: string) => void): () => void {
    this.valueListeners.add(fn);
    return () => this.valueListeners.delete(fn);
  }
  private emitStructure(): void {
    for (const fn of this.structureListeners) fn();
  }
  private emitValue(key: string): void {
    for (const fn of this.valueListeners) fn(key);
  }

  isEnabled(kind: TelemetryKind, name: string): boolean {
    return this.items.get(keyOf(kind, name))?.enabled ?? false;
  }

  get(kind: TelemetryKind, name: string): TelemetryRegistryItem | undefined {
    return this.items.get(keyOf(kind, name));
  }

  private storedPref(kind: TelemetryKind, name: string): boolean | null {
    const v = localStorage.getItem(LS_PREFIX + keyOf(kind, name));
    if (v === null) return null;
    return v === '1';
  }

  // Migrate a placeholder entry to its real name (buffer labeled after
  // allocation). Preserves enabled state and its persisted preference.
  private rename(kind: TelemetryKind, from: string, to: string): void {
    const fromKey = keyOf(kind, from);
    const old = this.items.get(fromKey);
    if (!old) return;
    const toKey = keyOf(kind, to);
    this.items.delete(fromKey);
    const fromLs = localStorage.getItem(LS_PREFIX + fromKey);
    localStorage.removeItem(LS_PREFIX + fromKey);
    if (!this.items.has(toKey)) {
      old.name = to;
      this.items.set(toKey, old);
      // Carry placeholder-set preferences over to the real name.
      if (fromLs !== null && localStorage.getItem(LS_PREFIX + toKey) === null) {
        localStorage.setItem(LS_PREFIX + toKey, fromLs);
      }
      const fromShape = localStorage.getItem(LS_SHAPE_PREFIX + fromKey);
      localStorage.removeItem(LS_SHAPE_PREFIX + fromKey);
      if (fromShape !== null && localStorage.getItem(LS_SHAPE_PREFIX + toKey) === null) {
        localStorage.setItem(LS_SHAPE_PREFIX + toKey, fromShape);
      }
      // Apply the destination's persisted shape hint.
      const toShape = localStorage.getItem(LS_SHAPE_PREFIX + toKey)?.match(/^(\d+)x(\d+)$/);
      if (toShape && !old.shapeRows) {
        old.shapeRows = parseInt(toShape[1], 10);
        old.shapeCols = parseInt(toShape[2], 10);
      }
      // Same for the resolution override.
      const fromDim = localStorage.getItem(LS_MAXDIM_PREFIX + fromKey);
      localStorage.removeItem(LS_MAXDIM_PREFIX + fromKey);
      if (fromDim !== null && localStorage.getItem(LS_MAXDIM_PREFIX + toKey) === null) {
        localStorage.setItem(LS_MAXDIM_PREFIX + toKey, fromDim);
      }
      const toDim = parseInt(localStorage.getItem(LS_MAXDIM_PREFIX + toKey) || '', 10);
      if (Number.isFinite(toDim) && toDim > 0 && !old.maxDim) old.maxDim = toDim;
      // Apply the destination name's own persisted preference — e.g. a buffer
      // the user enabled in a previous session: each new session declares it
      // under the probe's placeholder name first, so the pref saved under the
      // real name only becomes applicable at rename time.
      const pref = this.storedPref(kind, to);
      if (pref !== null && old.enabled !== pref) {
        old.enabled = pref;
        this.pushTrack(old);
      }
    }
    this.emitStructure();
  }

  // Register (or update metadata of) an item. Returns the item.
  declare(decl: TelemetryDecl): TelemetryRegistryItem {
    if (decl.renamedFrom && decl.renamedFrom !== decl.name) {
      this.rename(decl.kind, decl.renamedFrom, decl.name);
    }
    const k = keyOf(decl.kind, decl.name);
    let item = this.items.get(k);
    if (!item) {
      item = { kind: decl.kind, name: decl.name, enabled: false, meta: decl.meta };

      // Restore a persisted shape hint ("RxC") and resolution override.
      const shapeLs = localStorage.getItem(LS_SHAPE_PREFIX + k);
      const shapeMatch = shapeLs?.match(/^(\d+)x(\d+)$/);
      if (shapeMatch) {
        item.shapeRows = parseInt(shapeMatch[1], 10);
        item.shapeCols = parseInt(shapeMatch[2], 10);
      }
      const maxDimLs = parseInt(localStorage.getItem(LS_MAXDIM_PREFIX + k) || '', 10);
      if (Number.isFinite(maxDimLs) && maxDimLs > 0) item.maxDim = maxDimLs;

      // Restore persisted preference, or auto-check the first 3 scalars so the
      // panel (and loss chart) isn't empty by default.
      const pref = this.storedPref(decl.kind, decl.name);
      if (pref !== null) {
        item.enabled = pref;
      } else if (decl.kind === 'scalar') {
        this.scalarDiscoveryOrder.push(decl.name);
        if (this.scalarDiscoveryOrder.length <= AUTO_CHECK_SCALARS) item.enabled = true;
      }

      this.items.set(k, item);
      // Sync initial enabled state to main (so tracks replay to socket clients).
      if (item.enabled) this.pushTrack(item);
      this.emitStructure();
    } else if (decl.meta) {
      // Merge late-arriving metadata (e.g. buffer shape from its first frame).
      const merged = { ...item.meta, ...decl.meta };
      if (JSON.stringify(merged) !== JSON.stringify(item.meta)) {
        item.meta = merged;
        this.emitStructure();
      }
    }
    return item;
  }

  setEnabled(kind: TelemetryKind, name: string, enabled: boolean): void {
    const item = this.items.get(keyOf(kind, name));
    if (!item || item.enabled === enabled) return;
    item.enabled = enabled;
    localStorage.setItem(LS_PREFIX + keyOf(kind, name), enabled ? '1' : '0');
    this.pushTrack(item);
    this.emitStructure();
  }

  // User-provided tensor shape ("the buffer is really 256×384"). Forwarded to
  // the probe (affects future frames) and persisted per name.
  setShape(kind: TelemetryKind, name: string, rows: number, cols: number): void {
    if (!Number.isFinite(rows) || !Number.isFinite(cols) || rows < 1 || cols < 1) return;
    const item = this.declare({ kind, name });
    if (item.shapeRows === rows && item.shapeCols === cols) return;
    item.shapeRows = Math.floor(rows);
    item.shapeCols = Math.floor(cols);
    localStorage.setItem(LS_SHAPE_PREFIX + keyOf(kind, name), `${item.shapeRows}x${item.shapeCols}`);
    this.pushTrack(item);
    this.emitStructure();
  }

  getShape(kind: TelemetryKind, name: string): { rows: number; cols: number } | null {
    const item = this.items.get(keyOf(kind, name));
    return item?.shapeRows && item?.shapeCols
      ? { rows: item.shapeRows, cols: item.shapeCols }
      : null;
  }

  // Per-buffer streaming resolution (max cells per dimension), persisted.
  setMaxDim(kind: TelemetryKind, name: string, maxDim: number): void {
    const clamped = Math.max(16, Math.min(256, Math.floor(maxDim)));
    const item = this.declare({ kind, name });
    if (item.maxDim === clamped) return;
    item.maxDim = clamped;
    localStorage.setItem(LS_MAXDIM_PREFIX + keyOf(kind, name), String(clamped));
    this.pushTrack(item);
  }

  getMaxDim(kind: TelemetryKind, name: string): number {
    return this.items.get(keyOf(kind, name))?.maxDim ?? DEFAULT_MAX_DIM;
  }

  private pushTrack(item: TelemetryRegistryItem): void {
    forge.telemetry.setTrack({
      kind: item.kind,
      name: item.name,
      enabled: item.enabled,
      maxDim: item.maxDim ?? DEFAULT_MAX_DIM,
      ...(item.shapeRows && item.shapeCols
        ? { rows: item.shapeRows, cols: item.shapeCols }
        : {}),
    });
  }

  // ── Live data notes (update last-value readout) ──

  noteScalar(name: string, step: number, value: number): void {
    const item = this.declare({ kind: 'scalar', name });
    item.lastValue = value;
    item.lastStep = step;
    this.emitValue(keyOf('scalar', name));
  }

  noteTiming(name: string, ms: number, step: number): void {
    const item = this.declare({ kind: 'timer', name });
    item.lastValue = ms;
    item.lastStep = step;
    this.emitValue(keyOf('timer', name));
  }

  noteTensor(name: string, rows: number, cols: number, step: number): void {
    const item = this.declare({ kind: 'buffer', name, meta: { rows, cols } });
    item.lastRows = rows;
    item.lastCols = cols;
    item.lastStep = step;
    this.emitValue(keyOf('buffer', name));
  }
}

export const telemetryRegistry = new TelemetryRegistry();

// ── base64 float32 decode (single decode point for tensor frames) ──

function decodeFloat32(b64: string): Float32Array {
  try {
    const bin = atob(b64);
    const len = bin.length;
    const bytes = new Uint8Array(len);
    for (let i = 0; i < len; i++) bytes[i] = bin.charCodeAt(i);
    // Trim to a whole number of float32s to be safe.
    const floatLen = Math.floor(len / 4);
    return new Float32Array(bytes.buffer, 0, floatLen);
  } catch {
    return new Float32Array(0);
  }
}

// ── IPC wiring (once) ──

let ipcWired = false;
function setupTelemetryIpc(): void {
  if (ipcWired) return;
  ipcWired = true;

  forge.telemetry.onDecl((decl: TelemetryDecl) => {
    telemetryRegistry.declare(decl);
  });

  forge.telemetry.onTensor((t: TelemetryTensor) => {
    const frame: TensorFrame = {
      name: t.name,
      step: t.step,
      rows: t.rows,
      cols: t.cols,
      srcRows: t.srcRows,
      srcCols: t.srcCols,
      dtype: t.dtype,
      data: decodeFloat32(t.data),
    };
    // Register under SOURCE dims so the sidebar shows the true tensor shape.
    telemetryRegistry.noteTensor(t.name, t.srcRows ?? t.rows, t.srcCols ?? t.cols, t.step);
    // Publish for training-view (heatmap + ring buffer) and future 3D viz.
    store.set('tensorFrame', frame);
  });

  // Scalars/timers also flow through the legacy training channels; keep the
  // registry's last-value readout in sync.
  forge.build.onTrainingScalar((s: TrainingScalar) => {
    telemetryRegistry.noteScalar(s.name, s.step, s.value);
  });
  forge.build.onTrainingTiming((t: TrainingTimingEvent) => {
    telemetryRegistry.noteTiming(t.name, t.durationMs, t.step);
  });
}

// ─────────────────────────────────────────────────────────────────────────────
// Sidebar panel
// ─────────────────────────────────────────────────────────────────────────────

interface RowRefs {
  row: HTMLElement;
  checkbox: HTMLInputElement;
  value: HTMLElement;
}

class TelemetryPanel {
  private container: HTMLElement;
  private groups: Record<TelemetryKind, HTMLElement>;
  private emptyEls: Record<TelemetryKind, HTMLElement>;
  private rows = new Map<string, RowRefs>();
  private valueDirty = new Set<string>();
  private valueFlushScheduled = false;
  private openShapeEditor: HTMLElement | null = null;

  constructor(parent: HTMLElement) {
    this.container = document.createElement('div');
    this.container.className = 'telemetry-panel';

    // Header
    const header = document.createElement('div');
    header.className = 'telemetry-panel-header';
    const title = document.createElement('span');
    title.className = 'telemetry-panel-title';
    title.textContent = 'TELEMETRY';
    header.appendChild(title);
    this.container.appendChild(header);

    const content = document.createElement('div');
    content.className = 'telemetry-panel-content';

    this.groups = {} as Record<TelemetryKind, HTMLElement>;
    this.emptyEls = {} as Record<TelemetryKind, HTMLElement>;
    for (const [kind, label] of [
      ['scalar', 'SCALARS'],
      ['timer', 'TIMERS'],
      ['buffer', 'BUFFERS'],
    ] as [TelemetryKind, string][]) {
      content.appendChild(this.makeSectionTitle(label));
      const group = document.createElement('div');
      group.className = 'telemetry-group';
      const empty = document.createElement('div');
      empty.className = 'telemetry-empty';
      empty.textContent = 'none discovered';
      group.appendChild(empty);
      content.appendChild(group);
      this.groups[kind] = group;
      this.emptyEls[kind] = empty;
    }

    this.container.appendChild(content);
    parent.appendChild(this.container);

    // Show/hide alongside the training view (both appear when telemetry arrives).
    store.on('trainingViewVisible', (visible: boolean) => {
      this.container.classList.toggle('visible', visible);
    });
    if (store.get<boolean>('trainingViewVisible')) {
      this.container.classList.add('visible');
    }

    // Rebuild rows on structural change; update value text on value change.
    telemetryRegistry.onStructure(() => this.rebuild());
    telemetryRegistry.onValue((key) => this.markValueDirty(key));

    this.rebuild();
  }

  private makeSectionTitle(text: string): HTMLElement {
    const el = document.createElement('div');
    el.className = 'telemetry-section-title';
    el.textContent = text;
    return el;
  }

  // Reconcile rows against the registry. Rows are keyed by "kind name" so
  // existing rows (and their checkboxes) survive re-renders.
  private rebuild(): void {
    const seen = new Set<string>();
    const counts: Record<TelemetryKind, number> = { scalar: 0, timer: 0, buffer: 0 };

    for (const item of telemetryRegistry.items.values()) {
      const key = keyOf(item.kind, item.name);
      seen.add(key);
      counts[item.kind]++;

      let refs = this.rows.get(key);
      if (!refs) {
        refs = this.createRow(item.kind, item.name);
        this.rows.set(key, refs);
        this.groups[item.kind].appendChild(refs.row);
      }
      refs.checkbox.checked = item.enabled;
      refs.value.textContent = formatValue(item);
    }

    // Remove rows for items no longer present (shouldn't normally happen).
    for (const [key, refs] of this.rows) {
      if (!seen.has(key)) {
        refs.row.remove();
        this.rows.delete(key);
      }
    }

    for (const kind of ['scalar', 'timer', 'buffer'] as TelemetryKind[]) {
      this.emptyEls[kind].style.display = counts[kind] === 0 ? '' : 'none';
    }
  }

  private createRow(kind: TelemetryKind, name: string): RowRefs {
    const row = document.createElement('label');
    row.className = 'telemetry-row';

    const checkbox = document.createElement('input');
    checkbox.type = 'checkbox';
    checkbox.className = 'telemetry-checkbox';
    checkbox.addEventListener('change', () => {
      telemetryRegistry.setEnabled(kind, name, checkbox.checked);
    });
    row.appendChild(checkbox);

    const nameEl = document.createElement('span');
    nameEl.className = 'telemetry-name';
    nameEl.textContent = name;
    nameEl.title = name;
    row.appendChild(nameEl);

    const value = document.createElement('span');
    value.className = 'telemetry-value';
    row.appendChild(value);

    // Buffers: shape editor toggle (set the tensor's true rows × cols)
    if (kind === 'buffer') {
      const shapeBtn = document.createElement('button');
      shapeBtn.type = 'button';
      shapeBtn.className = 'telemetry-shape-btn';
      shapeBtn.textContent = '⊞';
      shapeBtn.title = 'Set tensor dimensions (rows × cols)';
      shapeBtn.addEventListener('click', (e) => {
        e.preventDefault(); // don't toggle the row's checkbox (row is a <label>)
        e.stopPropagation();
        this.toggleShapeEditor(row, name);
      });
      row.appendChild(shapeBtn);
    }

    return { row, checkbox, value };
  }

  // Inline rows×cols editor shown directly under a buffer row.
  private toggleShapeEditor(row: HTMLElement, name: string): void {
    const wasOpenFor = this.openShapeEditor?.dataset.bufferName;
    this.closeShapeEditor();
    if (wasOpenFor === name) return; // second click on same row = close

    const editor = document.createElement('div');
    editor.className = 'telemetry-shape-editor';
    editor.dataset.bufferName = name;

    const makeInput = (title: string, initial: number | undefined, placeholder: number | undefined) => {
      const input = document.createElement('input');
      input.type = 'number';
      input.min = '1';
      input.className = 'telemetry-shape-input';
      input.title = title;
      if (initial) input.value = String(initial);
      if (placeholder) input.placeholder = String(placeholder);
      return input;
    };

    const item = telemetryRegistry.get('buffer', name);
    const hint = telemetryRegistry.getShape('buffer', name);
    const rowsInput = makeInput('rows', hint?.rows, item?.lastRows);
    const colsInput = makeInput('cols', hint?.cols, item?.lastCols);

    const apply = () => {
      const r = parseInt(rowsInput.value, 10);
      const c = parseInt(colsInput.value, 10);
      if (Number.isFinite(r) && Number.isFinite(c) && r > 0 && c > 0) {
        telemetryRegistry.setShape('buffer', name, r, c);
      }
      this.closeShapeEditor();
    };

    for (const input of [rowsInput, colsInput]) {
      input.addEventListener('keydown', (e) => {
        e.stopPropagation();
        if (e.key === 'Enter') apply();
        else if (e.key === 'Escape') this.closeShapeEditor();
      });
    }

    const sep = document.createElement('span');
    sep.className = 'telemetry-shape-sep';
    sep.textContent = '×';

    const applyBtn = document.createElement('button');
    applyBtn.type = 'button';
    applyBtn.className = 'telemetry-shape-apply';
    applyBtn.textContent = 'set';
    applyBtn.title = 'Apply dimensions (takes effect on new frames)';
    applyBtn.addEventListener('click', apply);

    editor.appendChild(rowsInput);
    editor.appendChild(sep);
    editor.appendChild(colsInput);
    editor.appendChild(applyBtn);

    row.insertAdjacentElement('afterend', editor);
    this.openShapeEditor = editor;
    rowsInput.focus();
    rowsInput.select();
  }

  private closeShapeEditor(): void {
    this.openShapeEditor?.remove();
    this.openShapeEditor = null;
  }

  // Coalesce high-frequency value updates into one rAF flush.
  private markValueDirty(key: string): void {
    this.valueDirty.add(key);
    if (this.valueFlushScheduled) return;
    this.valueFlushScheduled = true;
    requestAnimationFrame(() => {
      this.valueFlushScheduled = false;
      for (const key of this.valueDirty) {
        const refs = this.rows.get(key);
        const item = telemetryRegistry.items.get(key);
        if (refs && item) refs.value.textContent = formatValue(item);
      }
      this.valueDirty.clear();
    });
  }
}

function formatValue(item: TelemetryRegistryItem): string {
  if (item.kind === 'buffer') {
    const r = item.lastRows ?? item.meta?.rows;
    const c = item.lastCols ?? item.meta?.cols;
    if (r && c) {
      return item.lastStep !== undefined ? `${r}×${c} @${item.lastStep}` : `${r}×${c}`;
    }
    return '—';
  }
  if (item.lastValue === undefined) return '—';
  if (item.kind === 'timer') {
    return item.lastValue >= 1000
      ? `${(item.lastValue / 1000).toFixed(2)}s`
      : `${item.lastValue.toFixed(1)}ms`;
  }
  // scalar
  const v = item.lastValue;
  const abs = Math.abs(v);
  if (abs !== 0 && (abs < 1e-3 || abs >= 1e6)) return v.toExponential(2);
  return v.toFixed(4);
}

// ── Public mount entry point ──
//
// Call once (from training-view.ts, which owns a real parent element). Accepts
// either a CSS selector or an element. Safe to call multiple times.
let panelInstance: TelemetryPanel | null = null;
export function initTelemetryPanel(parent: HTMLElement | string): void {
  setupTelemetryIpc();
  if (panelInstance) return;
  const parentEl =
    typeof parent === 'string'
      ? (document.querySelector(parent) as HTMLElement | null)
      : parent;
  if (!parentEl) {
    console.warn('[telemetry] initTelemetryPanel: parent not found:', parent);
    return;
  }
  panelInstance = new TelemetryPanel(parentEl);
}
