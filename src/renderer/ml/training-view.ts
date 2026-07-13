import { store } from '../state';
import type { TrainingScalar, TrainingTimingEvent, MemoryBarData } from '../../shared/types';
import { initTelemetryPanel, telemetryRegistry } from '../panels/telemetry-panel';
import type { TensorFrame } from '../panels/telemetry-panel';
import { button } from '../ui/components';
import { WeightGrid3D } from './weight-grid-3d';

const forge = (window as any).forge;

interface MemorySnapshot {
  heapUsed: number;
  timestamp: number;
}

interface Stats {
  min: number;
  max: number;
}

interface TimingRowRefs {
  row: HTMLElement;
  label: HTMLElement;
  bar: HTMLElement;
  value: HTMLElement;
}

interface TensorViewRefs {
  wrapper: HTMLElement;
  label: HTMLElement;
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
}

const MAX_POINTS = 1000;
const MAX_TENSOR_FRAMES = 32; // ring buffer depth per tracked buffer

export class TrainingView {
  private container: HTMLElement;
  private lossCanvas: HTMLCanvasElement;
  private lossCtx: CanvasRenderingContext2D;
  private memoryCanvas: HTMLCanvasElement;
  private memoryCtx: CanvasRenderingContext2D;
  private timingChartSection: HTMLElement;
  private timingChartCanvas: HTMLCanvasElement;
  private timingChartCtx: CanvasRenderingContext2D;
  private timingContainer: HTMLElement;
  private tensorSection: HTMLElement;
  private tensorContainer: HTMLElement;

  private scalars = new Map<string, TrainingScalar[]>();
  private timings: TrainingTimingEvent[] = [];
  private memoryHistory: MemorySnapshot[] = [];

  // Incrementally-tracked ranges (avoids spreading full arrays every frame).
  private scalarStats = new Map<string, Stats>();
  private prevScalarStats = new Map<string, Stats>();
  private memoryMax = 0;
  private prevMemoryMax = 0;

  // Tensor ring buffers (latest MAX_TENSOR_FRAMES frames per tracked buffer).
  private tensorFrames = new Map<string, TensorFrame[]>();
  private tensorViews = new Map<string, TensorViewRefs>();
  private tmpCanvas: HTMLCanvasElement;
  private tmpCtx: CanvasRenderingContext2D;

  // Dirty-flag render scheduling (render only when new data / visibility change).
  private dirty = false;
  private frameScheduled = false;
  private animFrame: number | null = null;

  // Reused timing-row element pool (no innerHTML rebuild).
  private timingRowPool: TimingRowRefs[] = [];

  // Previous run data for ghost overlay
  private prevScalars = new Map<string, TrainingScalar[]>();
  private prevTimings: TrainingTimingEvent[] = [];
  private prevMemoryHistory: MemorySnapshot[] = [];

  // 3D weight-grid overlay. Created eagerly (hidden, no GL until opened) so
  // its frame ring buffer fills from the first tensor frame — created lazily
  // it would open onto "no data" because frames that arrived before the first
  // click were never captured.
  private weightGrid: WeightGrid3D | null = null;

  constructor(parent: HTMLElement) {
    this.container = document.createElement('div');
    this.container.className = 'training-view';

    // Header
    const header = document.createElement('div');
    header.className = 'training-view-header';
    header.textContent = 'TRAINING';
    this.container.appendChild(header);

    // Loss curve canvas
    const lossSection = document.createElement('div');
    lossSection.className = 'training-section';
    const lossLabel = document.createElement('div');
    lossLabel.className = 'training-section-label';
    lossLabel.textContent = 'Loss';
    lossSection.appendChild(lossLabel);

    this.lossCanvas = document.createElement('canvas');
    this.lossCanvas.className = 'training-canvas';
    this.lossCanvas.width = 252;
    this.lossCanvas.height = 120;
    lossSection.appendChild(this.lossCanvas);
    this.lossCtx = this.lossCanvas.getContext('2d')!;
    this.container.appendChild(lossSection);

    // Memory timeline canvas
    const memSection = document.createElement('div');
    memSection.className = 'training-section';
    const memLabel = document.createElement('div');
    memLabel.className = 'training-section-label';
    memLabel.textContent = 'Memory';
    memSection.appendChild(memLabel);

    this.memoryCanvas = document.createElement('canvas');
    this.memoryCanvas.className = 'training-canvas';
    this.memoryCanvas.width = 252;
    this.memoryCanvas.height = 120;
    memSection.appendChild(this.memoryCanvas);
    this.memoryCtx = this.memoryCanvas.getContext('2d')!;
    this.container.appendChild(memSection);

    // Kernel time over steps — per-step curves for ENABLED timers (same
    // selection model as the loss chart: check timers in the TIMERS section)
    this.timingChartSection = document.createElement('div');
    this.timingChartSection.className = 'training-section';
    this.timingChartSection.style.display = 'none';
    const timingChartLabel = document.createElement('div');
    timingChartLabel.className = 'training-section-label';
    timingChartLabel.textContent = 'Kernel time (ms)';
    this.timingChartSection.appendChild(timingChartLabel);

    this.timingChartCanvas = document.createElement('canvas');
    this.timingChartCanvas.className = 'training-canvas';
    this.timingChartCanvas.width = 252;
    this.timingChartCanvas.height = 120;
    this.timingChartSection.appendChild(this.timingChartCanvas);
    this.timingChartCtx = this.timingChartCanvas.getContext('2d')!;
    this.container.appendChild(this.timingChartSection);

    // Operation timing breakdown
    const timingSection = document.createElement('div');
    timingSection.className = 'training-section';
    const timingLabel = document.createElement('div');
    timingLabel.className = 'training-section-label';
    timingLabel.textContent = 'Timing';
    timingSection.appendChild(timingLabel);

    this.timingContainer = document.createElement('div');
    this.timingContainer.className = 'training-timing';
    timingSection.appendChild(this.timingContainer);
    this.container.appendChild(timingSection);

    // Tensors section (heatmap previews of tracked GPU buffers)
    this.tensorSection = document.createElement('div');
    this.tensorSection.className = 'training-section';
    this.tensorSection.style.display = 'none';
    const tensorHeader = document.createElement('div');
    tensorHeader.style.cssText = 'display:flex;align-items:center;gap:8px;';
    const tensorLabel = document.createElement('div');
    tensorLabel.className = 'training-section-label';
    tensorLabel.textContent = 'Tensors';
    tensorHeader.appendChild(tensorLabel);
    const open3dBtn = button({
      label: '3D',
      icon: 'box',
      variant: 'ghost',
      size: 'sm',
      title: 'Open 3D weight grid',
      className: 'training-tensor-open',
      onClick: () => this.openWeightGrid(),
    });
    tensorHeader.appendChild(open3dBtn);
    this.tensorSection.appendChild(tensorHeader);
    this.tensorContainer = document.createElement('div');
    this.tensorContainer.style.cssText = 'display:flex;flex-direction:column;gap:8px;';
    this.tensorSection.appendChild(this.tensorContainer);
    this.container.appendChild(this.tensorSection);

    parent.appendChild(this.container);

    // Offscreen canvas for scaling small tensor grids up to the preview box.
    this.tmpCanvas = document.createElement('canvas');
    this.tmpCtx = this.tmpCanvas.getContext('2d')!;

    // Mount the telemetry selection panel into the same sidebar (this module
    // owns the mount so app.ts stays untouched — see telemetry-panel.ts).
    initTelemetryPanel(parent);

    // Eager: must subscribe to tensor frames from startup (see field comment).
    this.weightGrid = new WeightGrid3D(document.body);

    // Toggle visibility
    store.on('trainingViewVisible', (visible: boolean) => {
      this.container.classList.toggle('visible', visible);
      if (visible) this.markDirty();
      else this.stopRenderLoop();
    });

    // Re-plot when the selection (checkboxes) changes.
    telemetryRegistry.onStructure(() => this.markDirty());

    // Listen for training data
    forge.build.onTrainingScalar((data: TrainingScalar) => {
      let arr = this.scalars.get(data.name);
      if (!arr) {
        arr = [];
        this.scalars.set(data.name, arr);
      }
      arr.push(data);

      if (arr.length > MAX_POINTS) {
        arr.splice(0, arr.length - MAX_POINTS);
        this.recomputeScalarStats(data.name);
      } else {
        const st = this.scalarStats.get(data.name) || { min: Infinity, max: -Infinity };
        if (data.value < st.min) st.min = data.value;
        if (data.value > st.max) st.max = data.value;
        this.scalarStats.set(data.name, st);
      }

      this.ensureVisible();
      this.markDirty();
    });

    forge.build.onTrainingTiming((data: TrainingTimingEvent) => {
      this.timings.push(data);

      // Keep last 500 entries PER timer name to avoid one timer evicting another
      if (this.timings.length > 5000) {
        const counts = new Map<string, number>();
        for (let i = this.timings.length - 1; i >= 0; i--) {
          const n = this.timings[i].name;
          const c = (counts.get(n) || 0) + 1;
          counts.set(n, c);
          if (c > 500) {
            this.timings.splice(i, 1);
          }
        }
      }

      this.ensureVisible();
      this.markDirty();
    });

    // Decoded GPU-buffer frames (published by telemetry-panel on the store).
    store.on('tensorFrame', (frame: TensorFrame) => {
      let frames = this.tensorFrames.get(frame.name);
      if (!frames) {
        frames = [];
        this.tensorFrames.set(frame.name, frames);
      }
      frames.push(frame);
      if (frames.length > MAX_TENSOR_FRAMES) {
        frames.splice(0, frames.length - MAX_TENSOR_FRAMES);
      }
      // Buffers alone (e.g. a debug session with no scalars) should surface
      // the sidebar too, so filled weight buffers render as they arrive.
      this.ensureVisible();
      this.markDirty();
    });

    // Repaint canvases when the light/dark theme flips (canvas colors are
    // resolved at draw time, not via CSS).
    store.on('themeMode', () => this.markDirty());

    // Sample live heap data from memoryBar store
    store.on('memoryBar', (data: MemoryBarData) => {
      if (data.heapUsed > 0) {
        this.memoryHistory.push({ heapUsed: data.heapUsed, timestamp: Date.now() });
        if (this.memoryHistory.length > MAX_POINTS) {
          this.memoryHistory.splice(0, this.memoryHistory.length - MAX_POINTS);
          this.memoryMax = 0;
          for (const s of this.memoryHistory) if (s.heapUsed > this.memoryMax) this.memoryMax = s.heapUsed;
        } else if (data.heapUsed > this.memoryMax) {
          this.memoryMax = data.heapUsed;
        }
        this.markDirty();
      }
    });
  }

  private recomputeScalarStats(name: string): void {
    const arr = this.scalars.get(name);
    if (!arr || arr.length === 0) {
      this.scalarStats.delete(name);
      return;
    }
    let min = Infinity;
    let max = -Infinity;
    for (const p of arr) {
      if (p.value < min) min = p.value;
      if (p.value > max) max = p.value;
    }
    this.scalarStats.set(name, { min, max });
  }

  // Only performs store writes on the visibility transition (not every event).
  private ensureVisible(): void {
    if (!this.container.classList.contains('visible')) {
      this.container.classList.add('visible');
      store.set('trainingViewVisible', true);
      store.set('runtimeVisible', true);
    }
  }

  clear(): void {
    // Save current run as previous before clearing
    if (this.scalars.size > 0 || this.timings.length > 0 || this.memoryHistory.length > 0) {
      this.prevScalars = new Map(this.scalars);
      this.prevTimings = [...this.timings];
      this.prevMemoryHistory = [...this.memoryHistory];
      this.prevScalarStats = new Map(this.scalarStats);
      this.prevMemoryMax = this.memoryMax;
    }
    this.scalars.clear();
    this.timings = [];
    this.memoryHistory = [];
    this.scalarStats.clear();
    this.memoryMax = 0;
    this.tensorFrames.clear();
    this.lossCtx.clearRect(0, 0, this.lossCanvas.width, this.lossCanvas.height);
    this.memoryCtx.clearRect(0, 0, this.memoryCanvas.width, this.memoryCanvas.height);
    this.timingChartCtx.clearRect(0, 0, this.timingChartCanvas.width, this.timingChartCanvas.height);
    this.timingContainer.innerHTML = '';
    this.timingRowPool = [];
    for (const view of this.tensorViews.values()) view.wrapper.remove();
    this.tensorViews.clear();
    this.tensorSection.style.display = 'none';
    this.markDirty();
  }

  // ── Dirty-flag render scheduling ──

  private markDirty(): void {
    this.dirty = true;
    this.scheduleFrame();
  }

  private scheduleFrame(): void {
    if (this.frameScheduled) return;
    if (!this.container.classList.contains('visible')) return;
    this.frameScheduled = true;
    this.animFrame = requestAnimationFrame(() => {
      this.animFrame = null;
      this.frameScheduled = false;
      if (!this.dirty) return;
      this.dirty = false;
      this.renderLossChart();
      this.renderMemoryChart();
      this.renderTimingChart();
      this.renderTimingBreakdown();
      this.renderTensors();
    });
  }

  private stopRenderLoop(): void {
    if (this.animFrame !== null) {
      cancelAnimationFrame(this.animFrame);
      this.animFrame = null;
    }
    this.frameScheduled = false;
  }

  private renderLossChart(): void {
    const ctx = this.lossCtx;
    const w = this.lossCanvas.width;
    const h = this.lossCanvas.height;
    ctx.clearRect(0, 0, w, h);

    // Draw grid
    ctx.strokeStyle = chartGridColor();
    ctx.lineWidth = 1;
    for (let i = 0; i < 5; i++) {
      const y = (h / 5) * i;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
    }

    const colors = chartSeriesColors();

    // Only plot ENABLED, non-memory scalars.
    const names = new Set<string>();
    for (const name of this.scalars.keys()) {
      if (isMemoryName(name)) continue;
      if (!telemetryRegistry.isEnabled('scalar', name)) continue;
      names.add(name);
    }
    for (const name of this.prevScalars.keys()) {
      if (isMemoryName(name)) continue;
      if (!telemetryRegistry.isEnabled('scalar', name)) continue;
      names.add(name);
    }

    let colorIdx = 0;
    for (const name of names) {
      const current = this.scalars.get(name);
      const prev = this.prevScalars.get(name);
      const color = colors[colorIdx++ % colors.length];

      // Combined min/max from incrementally-tracked stats (no array spread).
      let minVal = Infinity;
      let maxVal = -Infinity;
      const cs = this.scalarStats.get(name);
      if (cs && current && current.length >= 2) {
        minVal = Math.min(minVal, cs.min);
        maxVal = Math.max(maxVal, cs.max);
      }
      const ps = this.prevScalarStats.get(name);
      if (ps && prev && prev.length >= 2) {
        minVal = Math.min(minVal, ps.min);
        maxVal = Math.max(maxVal, ps.max);
      }
      if (!Number.isFinite(minVal) || !Number.isFinite(maxVal)) continue;
      const range = maxVal - minVal || 1;

      const drawLine = (points: TrainingScalar[] | undefined, strokeColor: string, lineWidth: number) => {
        if (!points || points.length < 2) return;
        ctx.strokeStyle = strokeColor;
        ctx.lineWidth = lineWidth;
        ctx.beginPath();
        for (let i = 0; i < points.length; i++) {
          const x = (i / (points.length - 1)) * w;
          const y = h - ((points[i].value - minVal) / range) * (h - 16) - 8;
          if (i === 0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        }
        ctx.stroke();
      };

      // Ghost previous run, then current run.
      drawLine(prev, color + '40', 1);
      drawLine(current, color, 1.5);

      // Label
      ctx.fillStyle = color;
      ctx.font = '10px JetBrains Mono, monospace';
      const lastArr = current && current.length > 0 ? current : prev;
      if (lastArr && lastArr.length > 0) {
        const lastVal = lastArr[lastArr.length - 1].value;
        ctx.fillText(`${name}: ${lastVal.toFixed(4)}`, 4, 12 + colorIdx * 12);
      }
    }
  }

  private renderMemoryChart(): void {
    const ctx = this.memoryCtx;
    const w = this.memoryCanvas.width;
    const h = this.memoryCanvas.height;
    ctx.clearRect(0, 0, w, h);

    // Draw grid
    ctx.strokeStyle = chartGridColor();
    ctx.lineWidth = 1;
    for (let i = 0; i < 5; i++) {
      const y = (h / 5) * i;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
    }

    // Use memory-named scalars if available, otherwise use live heap history.
    let memName = '';
    for (const name of this.scalars.keys()) {
      if (isMemoryName(name)) { memName = name; break; }
    }

    let values: TrainingScalar[] | MemorySnapshot[] = [];
    let prevValues: TrainingScalar[] | MemorySnapshot[] = [];
    let label = 'Heap';
    let maxVal = 1;
    let pick: (v: any) => number;

    if (memName) {
      values = this.scalars.get(memName) || [];
      label = memName;
      const ps = this.prevScalars.get(memName);
      prevValues = ps || [];
      pick = (v: TrainingScalar) => v.value;
      const cs = this.scalarStats.get(memName);
      const pcs = this.prevScalarStats.get(memName);
      maxVal = Math.max(cs?.max ?? 0, pcs?.max ?? 0) || 1;
    } else {
      values = this.memoryHistory;
      prevValues = this.prevMemoryHistory;
      pick = (v: MemorySnapshot) => v.heapUsed;
      maxVal = Math.max(this.memoryMax, this.prevMemoryMax) || 1;
    }

    if (values.length < 2 && prevValues.length < 2) return;

    const drawMemLine = (vals: any[], color: string, lw: number, fill: boolean) => {
      if (vals.length < 2) return;
      ctx.strokeStyle = color;
      ctx.lineWidth = lw;
      ctx.beginPath();
      for (let i = 0; i < vals.length; i++) {
        const x = (i / (vals.length - 1)) * w;
        const y = h - (pick(vals[i]) / maxVal) * (h - 16) - 8;
        if (i === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      }
      ctx.stroke();
      if (fill) {
        ctx.fillStyle = memFillColor();
        ctx.lineTo(w, h);
        ctx.lineTo(0, h);
        ctx.closePath();
        ctx.fill();
      }
    };

    drawMemLine(prevValues, memAccentColor() + '40', 1, false);
    drawMemLine(values, memAccentColor(), 1.5, true);

    // Label
    ctx.fillStyle = memAccentColor();
    ctx.font = '10px JetBrains Mono, monospace';
    const lastVal = values.length > 0 ? pick(values[values.length - 1]) : 0;
    ctx.fillText(`${label}: ${formatBytes(lastVal)}`, 4, 12);
  }

  // Per-step curves for enabled timers (e.g. one line per GPU kernel), so a
  // pipeline's performance is visible across the run — same selection and
  // ghost-previous-run semantics as the loss chart.
  private renderTimingChart(): void {
    const ctx = this.timingChartCtx;
    const w = this.timingChartCanvas.width;
    const h = this.timingChartCanvas.height;

    // Group current + previous run timings by ENABLED timer name.
    const groupEnabled = (events: TrainingTimingEvent[]): Map<string, TrainingTimingEvent[]> => {
      const out = new Map<string, TrainingTimingEvent[]>();
      for (const t of events) {
        if (!telemetryRegistry.isEnabled('timer', t.name)) continue;
        let arr = out.get(t.name);
        if (!arr) {
          arr = [];
          out.set(t.name, arr);
        }
        arr.push(t);
      }
      return out;
    };
    const current = groupEnabled(this.timings);
    const prev = groupEnabled(this.prevTimings);

    const plottable = [...current.values(), ...prev.values()].some((a) => a.length >= 2);
    if (!plottable) {
      this.timingChartSection.style.display = 'none';
      return;
    }
    this.timingChartSection.style.display = '';

    ctx.clearRect(0, 0, w, h);
    ctx.strokeStyle = chartGridColor();
    ctx.lineWidth = 1;
    for (let i = 0; i < 5; i++) {
      const y = (h / 5) * i;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
    }

    // Shared ms scale across all plotted series (timers share a unit, unlike
    // scalars) so relative kernel cost is readable at a glance.
    let maxMs = 0;
    for (const arr of [...current.values(), ...prev.values()]) {
      for (const t of arr) if (t.durationMs > maxMs) maxMs = t.durationMs;
    }
    if (maxMs <= 0) maxMs = 1;

    const colors = chartSeriesColors();
    const names = new Set<string>([...current.keys(), ...prev.keys()]);
    let colorIdx = 0;
    let labelIdx = 0;
    for (const name of names) {
      const color = colors[colorIdx++ % colors.length];

      const drawLine = (points: TrainingTimingEvent[] | undefined, strokeColor: string, lineWidth: number) => {
        if (!points || points.length < 2) return;
        ctx.strokeStyle = strokeColor;
        ctx.lineWidth = lineWidth;
        ctx.beginPath();
        for (let i = 0; i < points.length; i++) {
          const x = (i / (points.length - 1)) * w;
          const y = h - (points[i].durationMs / maxMs) * (h - 16) - 8;
          if (i === 0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        }
        ctx.stroke();
      };

      // Ghost previous run, then current run.
      drawLine(prev.get(name), color + '40', 1);
      drawLine(current.get(name), color, 1.5);

      const arr = current.get(name) ?? prev.get(name);
      if (arr && arr.length > 0) {
        ctx.fillStyle = color;
        ctx.font = '10px JetBrains Mono, monospace';
        const last = arr[arr.length - 1].durationMs;
        ctx.fillText(`${name}: ${last.toFixed(3)}ms`, 4, 12 + labelIdx++ * 12);
      }
    }
  }

  private renderTimingBreakdown(): void {
    // Aggregate ENABLED timers by operation name.
    const aggregated = new Map<string, { totalMs: number; count: number }>();
    for (const t of this.timings) {
      if (!telemetryRegistry.isEnabled('timer', t.name)) continue;
      const existing = aggregated.get(t.name) || { totalMs: 0, count: 0 };
      existing.totalMs += t.durationMs;
      existing.count++;
      aggregated.set(t.name, existing);
    }

    const sorted = Array.from(aggregated.entries())
      .sort(([, a], [, b]) => b.totalMs - a.totalMs)
      .slice(0, 8);

    const maxTime = sorted[0]?.[1].totalMs || 1;

    // Reuse pooled rows (no innerHTML rebuild).
    for (let i = 0; i < sorted.length; i++) {
      const [name, data] = sorted[i];
      const refs = this.ensureTimingRow(i);
      refs.row.style.display = '';
      refs.label.textContent = name;
      refs.bar.style.width = `${(data.totalMs / maxTime) * 100}%`;
      const avgMs = data.totalMs / data.count;
      refs.value.textContent = avgMs >= 1000
        ? `${(avgMs / 1000).toFixed(1)}s`
        : `${avgMs.toFixed(1)}ms`;
    }
    // Hide unused pooled rows.
    for (let i = sorted.length; i < this.timingRowPool.length; i++) {
      this.timingRowPool[i].row.style.display = 'none';
    }
  }

  private ensureTimingRow(index: number): TimingRowRefs {
    let refs = this.timingRowPool[index];
    if (refs) return refs;

    const row = document.createElement('div');
    row.className = 'timing-row';

    const label = document.createElement('span');
    label.className = 'timing-label';
    row.appendChild(label);

    const barOuter = document.createElement('div');
    barOuter.className = 'timing-bar-outer';
    const barInner = document.createElement('div');
    barInner.className = 'timing-bar-inner';
    barOuter.appendChild(barInner);
    row.appendChild(barOuter);

    const value = document.createElement('span');
    value.className = 'timing-value';
    row.appendChild(value);

    this.timingContainer.appendChild(row);
    refs = { row, label, bar: barInner, value };
    this.timingRowPool[index] = refs;
    return refs;
  }

  // ── Tensor heatmaps ──

  private renderTensors(): void {
    const active = new Set<string>();

    for (const [name, frames] of this.tensorFrames) {
      if (frames.length === 0) continue;
      if (!telemetryRegistry.isEnabled('buffer', name)) continue;
      active.add(name);

      const frame = frames[frames.length - 1];
      let view = this.tensorViews.get(name);
      if (!view) {
        view = this.createTensorView(name);
        this.tensorViews.set(name, view);
        this.tensorContainer.appendChild(view.wrapper);
      }
      let mn = Infinity, mx = -Infinity;
      for (let i = 0; i < frame.rows * frame.cols && i < frame.data.length; i++) {
        const v = frame.data[i];
        if (v < mn) mn = v;
        if (v > mx) mx = v;
      }
      const range = Number.isFinite(mn) ? `  [${fmtVal(mn)}…${fmtVal(mx)}]` : '';
      const dimR = frame.srcRows ?? frame.rows;
      const dimC = frame.srcCols ?? frame.cols;
      view.label.textContent = `${name}  ${dimR}×${dimC} @${frame.step}${range}`;
      this.drawHeatmap(view, frame);
    }

    // Remove views whose buffer is no longer enabled / has no data.
    for (const [name, view] of this.tensorViews) {
      if (!active.has(name)) {
        view.wrapper.remove();
        this.tensorViews.delete(name);
      }
    }

    this.tensorSection.style.display = active.size > 0 ? '' : 'none';
  }

  // Lazily create + show the 3D weight-grid overlay, focused on `name`.
  private openWeightGrid(name?: string): void {
    if (!this.weightGrid) {
      this.weightGrid = new WeightGrid3D(document.body);
    }
    this.weightGrid.open(name);
  }

  private createTensorView(name: string): TensorViewRefs {
    const wrapper = document.createElement('div');
    wrapper.style.cssText = 'display:flex;flex-direction:column;gap:3px;cursor:pointer;';
    wrapper.title = `Open ${name} in 3D`;
    wrapper.addEventListener('click', () => this.openWeightGrid(name));

    const label = document.createElement('div');
    label.className = 'training-section-label';
    label.style.cssText = 'opacity:0.7;font-size:9px;';
    label.textContent = name;
    wrapper.appendChild(label);

    const canvas = document.createElement('canvas');
    canvas.width = 236;
    canvas.height = 120;
    canvas.style.cssText = 'width:100%;border-radius:4px;image-rendering:pixelated;';
    wrapper.appendChild(canvas);

    const ctx = canvas.getContext('2d')!;
    return { wrapper, label, canvas, ctx };
  }

  private drawHeatmap(view: TensorViewRefs, frame: TensorFrame): void {
    const rows = Math.max(1, frame.rows | 0);
    const cols = Math.max(1, frame.cols | 0);
    const data = frame.data;
    if (data.length < rows * cols) return;

    // Diverging colormap centered at 0 → normalize by max absolute value.
    let maxAbs = 0;
    for (let i = 0; i < rows * cols; i++) {
      const a = Math.abs(data[i]);
      if (a > maxAbs) maxAbs = a;
    }

    // Render the raw grid into an offscreen canvas, then scale up.
    this.tmpCanvas.width = cols;
    this.tmpCanvas.height = rows;
    const img = this.tmpCtx.createImageData(cols, rows);
    const px = img.data;
    for (let i = 0; i < rows * cols; i++) {
      const [r, g, b] = diverging(data[i], maxAbs);
      const o = i * 4;
      px[o] = r;
      px[o + 1] = g;
      px[o + 2] = b;
      px[o + 3] = 255;
    }
    this.tmpCtx.putImageData(img, 0, 0);

    // Keep the SOURCE aspect ratio inside the fixed-width preview box (pooling
    // caps each dim independently, so pooled data can be square for a
    // non-square tensor).
    const aspectR = frame.srcRows ?? rows;
    const aspectC = frame.srcCols ?? cols;
    const cw = view.canvas.width;
    const ch = Math.min(180, Math.max(40, Math.round((cw * aspectR) / aspectC)));
    if (view.canvas.height !== ch) view.canvas.height = ch;
    view.ctx.imageSmoothingEnabled = false;
    view.ctx.clearRect(0, 0, cw, view.canvas.height);
    view.ctx.drawImage(this.tmpCanvas, 0, 0, cols, rows, 0, 0, cw, view.canvas.height);
  }
}

// Diverging blue → white → red colormap centered at 0.
function diverging(v: number, maxAbs: number): [number, number, number] {
  if (maxAbs <= 0 || !Number.isFinite(v)) return [255, 255, 255];
  const t = Math.max(-1, Math.min(1, v / maxAbs));
  if (t >= 0) {
    // white → red
    const k = Math.round(255 * (1 - t));
    return [255, k, k];
  }
  // white → blue
  const k = Math.round(255 * (1 + t)); // t is negative
  return [k, k, 255];
}

// ── Theme-aware chart colors ──
// The loss/memory charts render to canvas, which CSS variables can't reach —
// resolve colors from the current mode at draw time (a themeMode store event
// triggers a redraw on toggle).
function isLightMode(): boolean {
  return document.body.classList.contains('light-mode');
}
function chartGridColor(): string {
  return isLightMode() ? 'rgba(0,0,0,0.07)' : 'rgba(255,255,255,0.04)';
}
function chartSeriesColors(): string[] {
  return isLightMode()
    ? ['#2E7D5B', '#2F5FD0', '#9A6700', '#B3403F', '#6E5494']
    : ['#56B389', '#8DB2FF', '#D4A76A', '#CF6B6B', '#9BB5CF'];
}
function memAccentColor(): string {
  return isLightMode() ? '#2E7D5B' : '#56B389';
}
function memFillColor(): string {
  return isLightMode() ? 'rgba(46, 125, 91, 0.10)' : 'rgba(86, 179, 137, 0.10)';
}

// Compact numeric formatting for heatmap range labels.
function fmtVal(v: number): string {
  if (!Number.isFinite(v)) return '?';
  const a = Math.abs(v);
  if (a !== 0 && (a >= 10000 || a < 0.001)) return v.toExponential(1);
  return String(Math.round(v * 1000) / 1000);
}

function isMemoryName(name: string): boolean {
  const n = name.toLowerCase();
  return n.includes('memory') || n.includes('heap');
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${Math.round(bytes)}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
}
