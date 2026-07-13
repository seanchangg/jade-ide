import { store } from '../state';
import type { MemoryBarData } from '../../shared/types';

interface RunRecord {
  duration: number;
  heapUsed: number;
  peakAllocation: number;
  allocCount: number;
  timestamp: number;
}

interface BenchmarkEntry {
  name: string;
  flags: string;
  duration: number;
  peakAllocation: number;
  allocCount: number;
  timestamp: number;
}

export class RuntimePanel {
  private container: HTMLElement;
  private durationEl: HTMLElement;
  private bestEl: HTMLElement;
  private deltaEl: HTMLElement;
  private heapEl: HTMLElement;
  private peakEl: HTMLElement;
  private allocsEl: HTMLElement;
  private freesEl: HTMLElement;
  private leaksEl: HTMLElement;
  private hotspotContainer: HTMLElement;
  private historyContainer: HTMLElement;
  private benchmarkContainer: HTMLElement;

  private runHistory: RunRecord[] = [];
  private benchmarks: BenchmarkEntry[] = [];
  private personalBest = Infinity;
  private runTimer: ReturnType<typeof setInterval> | null = null;
  private runStartTime = 0;
  private visible = false;

  constructor(parent: HTMLElement) {
    this.container = document.createElement('div');
    this.container.className = 'runtime-panel';

    // Header
    const header = document.createElement('div');
    header.className = 'runtime-panel-header';
    const title = document.createElement('span');
    title.className = 'runtime-panel-title';
    title.textContent = 'RUNTIME';
    header.appendChild(title);
    const closeBtn = document.createElement('span');
    closeBtn.className = 'runtime-panel-close';
    closeBtn.textContent = '×';
    closeBtn.addEventListener('click', () => store.set('runtimeVisible', false));
    header.appendChild(closeBtn);
    this.container.appendChild(header);

    // Scrollable content
    const content = document.createElement('div');
    content.className = 'runtime-panel-content';

    // Speed section
    content.appendChild(this.makeSection('SPEED'));
    const speedRows = document.createElement('div');
    speedRows.className = 'runtime-rows';
    this.durationEl = this.addRow(speedRows, 'Duration', '—');
    this.bestEl = this.addRow(speedRows, 'Best', '—');
    this.deltaEl = this.addRow(speedRows, 'vs Last', '—');
    content.appendChild(speedRows);

    // Memory section
    content.appendChild(this.makeSection('MEMORY'));
    const memRows = document.createElement('div');
    memRows.className = 'runtime-rows';
    this.heapEl = this.addRow(memRows, 'Heap', '—');
    this.peakEl = this.addRow(memRows, 'Peak', '—');
    this.allocsEl = this.addRow(memRows, 'Allocs', '—');
    this.freesEl = this.addRow(memRows, 'Frees', '—');
    this.leaksEl = this.addRow(memRows, 'Leaks', '—');
    content.appendChild(memRows);

    // Hotspots section
    content.appendChild(this.makeSection('HOTSPOTS'));
    this.hotspotContainer = document.createElement('div');
    this.hotspotContainer.className = 'runtime-rows';
    this.hotspotContainer.innerHTML = '<div class="runtime-row"><span class="runtime-label" style="opacity:0.4">Build + Run with flow on</span></div>';
    content.appendChild(this.hotspotContainer);

    // Benchmarks section
    content.appendChild(this.makeSection('BENCHMARKS'));
    this.benchmarkContainer = document.createElement('div');
    this.benchmarkContainer.className = 'runtime-rows';
    this.benchmarkContainer.innerHTML = '<div class="runtime-row"><span class="runtime-label" style="opacity:0.4">Save a run from history</span></div>';
    content.appendChild(this.benchmarkContainer);

    // Run history section
    content.appendChild(this.makeSection('HISTORY'));
    this.historyContainer = document.createElement('div');
    this.historyContainer.className = 'runtime-rows';
    this.historyContainer.innerHTML = '<div class="runtime-row"><span class="runtime-label" style="opacity:0.4">No runs yet</span></div>';
    content.appendChild(this.historyContainer);

    this.container.appendChild(content);
    parent.appendChild(this.container);

    // Toggle visibility
    store.on('runtimeVisible', (visible: boolean) => {
      this.visible = visible;
      this.container.classList.toggle('visible', visible);
      // Re-sync memory readout on show (updates were skipped while hidden)
      if (visible) {
        const data = store.get<MemoryBarData>('memoryBar');
        if (data) this.updateMemory(data);
      }
    });

    // Subscribe to memory data. memoryBar can update every frame during a run;
    // skip the DOM writes entirely while the panel is hidden.
    store.on('memoryBar', (data: MemoryBarData) => {
      if (!this.visible) return;
      this.updateMemory(data);
    });

    // Subscribe to executed lines for hotspots
    store.on('executedLines', () => this.updateHotspots());

    // Live elapsed timer during execution
    store.on('isRunning', (running: boolean) => {
      if (running) {
        this.runStartTime = Date.now();
        this.durationEl.className = 'runtime-value';
        this.runTimer = setInterval(() => {
          const elapsed = Date.now() - this.runStartTime;
          this.durationEl.textContent = formatDuration(elapsed);
        }, 100);
      } else if (this.runTimer) {
        clearInterval(this.runTimer);
        this.runTimer = null;
      }
    });

    // Load persisted benchmarks
    store.on('benchmarks', (bm: BenchmarkEntry[]) => {
      if (bm) {
        this.benchmarks = bm;
        this.renderBenchmarks();
      }
    });
    const saved = store.get<BenchmarkEntry[]>('benchmarks');
    if (saved && saved.length > 0) {
      this.benchmarks = saved;
      this.renderBenchmarks();
    }
  }

  private updateMemory(data: MemoryBarData): void {
    this.heapEl.textContent = formatBytes(data.heapUsed);
    this.peakEl.textContent = formatBytes(data.peakAllocation);
    this.allocsEl.textContent = String(data.allocCount);
    this.freesEl.textContent = String(data.freeCount);
    this.leaksEl.textContent = String(data.leakCount);
    this.leaksEl.className = data.leakCount > 0 ? 'runtime-value error' : 'runtime-value';
  }

  recordRun(duration: number): void {
    const memBar = store.get<MemoryBarData>('memoryBar');
    const record: RunRecord = {
      duration,
      heapUsed: memBar?.heapUsed || 0,
      peakAllocation: memBar?.peakAllocation || 0,
      allocCount: memBar?.allocCount || 0,
      timestamp: Date.now(),
    };

    if (duration < this.personalBest) this.personalBest = duration;

    this.durationEl.textContent = formatDuration(duration);

    this.bestEl.textContent = formatDuration(this.personalBest);
    this.bestEl.className = duration === this.personalBest ? 'runtime-value accent' : 'runtime-value';

    if (this.runHistory.length > 0) {
      const last = this.runHistory[this.runHistory.length - 1];
      const diff = duration - last.duration;
      if (diff < 0) {
        this.deltaEl.textContent = `${formatDuration(Math.abs(diff))} faster`;
        this.deltaEl.className = 'runtime-value accent';
      } else if (diff > 0) {
        this.deltaEl.textContent = `${formatDuration(diff)} slower`;
        this.deltaEl.className = 'runtime-value error';
      } else {
        this.deltaEl.textContent = 'same';
        this.deltaEl.className = 'runtime-value';
      }
    }

    this.runHistory.push(record);
    this.updateHistory();
  }

  // ── Benchmarks ──

  private saveBenchmark(record: RunRecord, runIndex: number): void {
    const flags = (document.getElementById('custom-flags') as HTMLInputElement)?.value.trim() || '';
    const defaultName = `#${runIndex}${flags ? ' ' + flags : ''}`;

    // Show inline input in the benchmark section
    const inputRow = document.createElement('div');
    inputRow.className = 'benchmark-row';
    const input = document.createElement('input');
    input.className = 'benchmark-name-input';
    input.value = defaultName;
    input.placeholder = 'Benchmark name';
    input.spellcheck = false;
    inputRow.appendChild(input);

    this.benchmarkContainer.insertBefore(inputRow, this.benchmarkContainer.firstChild);
    input.focus();
    input.select();

    const commit = () => {
      const name = input.value.trim();
      inputRow.remove();
      if (!name) return;

      const entry: BenchmarkEntry = {
        name,
        flags,
        duration: record.duration,
        peakAllocation: record.peakAllocation,
        allocCount: record.allocCount,
        timestamp: record.timestamp,
      };

      this.benchmarks.push(entry);
      store.set('benchmarks', [...this.benchmarks]);
      this.renderBenchmarks();
    };

    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); commit(); }
      if (e.key === 'Escape') { e.preventDefault(); inputRow.remove(); }
    });
    input.addEventListener('blur', commit);
  }

  private deleteBenchmark(idx: number): void {
    this.benchmarks.splice(idx, 1);
    store.set('benchmarks', [...this.benchmarks]);
    this.renderBenchmarks();
  }

  private renderBenchmarks(): void {
    this.benchmarkContainer.innerHTML = '';

    if (this.benchmarks.length === 0) {
      this.benchmarkContainer.innerHTML = '<div class="runtime-row"><span class="runtime-label" style="opacity:0.4">Save a run from history</span></div>';
      return;
    }

    // Sort by duration for easy comparison
    const sorted = this.benchmarks
      .map((b, i) => ({ ...b, idx: i }))
      .sort((a, b) => a.duration - b.duration);

    const fastest = sorted[0].duration;

    for (const bm of sorted) {
      const row = document.createElement('div');
      row.className = 'benchmark-row';

      const nameEl = document.createElement('span');
      nameEl.className = 'benchmark-name';
      nameEl.textContent = bm.name;
      nameEl.title = bm.flags ? `flags: ${bm.flags}` : 'no extra flags';
      row.appendChild(nameEl);

      const statsEl = document.createElement('span');
      statsEl.className = 'benchmark-stats';

      const durEl = document.createElement('span');
      durEl.className = bm.duration === fastest ? 'benchmark-dur accent' : 'benchmark-dur';
      durEl.textContent = formatDuration(bm.duration);
      statsEl.appendChild(durEl);

      const memEl = document.createElement('span');
      memEl.className = 'benchmark-mem';
      memEl.textContent = formatBytes(bm.peakAllocation);
      statsEl.appendChild(memEl);

      row.appendChild(statsEl);

      // Compare with last run
      if (this.runHistory.length > 0) {
        const lastRun = this.runHistory[this.runHistory.length - 1];
        const diff = lastRun.duration - bm.duration;
        const tag = document.createElement('span');
        tag.className = 'benchmark-delta';
        if (Math.abs(diff) < 1) {
          tag.textContent = '=';
        } else if (diff < 0) {
          tag.textContent = `${formatDuration(Math.abs(diff))}↓`;
          tag.classList.add('accent');
        } else {
          tag.textContent = `${formatDuration(diff)}↑`;
          tag.classList.add('error');
        }
        tag.title = 'vs last run';
        row.appendChild(tag);
      }

      // Delete button
      const delBtn = document.createElement('span');
      delBtn.className = 'benchmark-del';
      delBtn.textContent = '×';
      delBtn.title = 'Remove benchmark';
      delBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        this.deleteBenchmark(bm.idx);
      });
      row.appendChild(delBtn);

      this.benchmarkContainer.appendChild(row);
    }
  }

  // ── Hotspots ──

  private updateHotspots(): void {
    const executedLines = store.get<Map<number, number>>('executedLines');
    if (!executedLines || executedLines.size === 0) {
      this.hotspotContainer.innerHTML = '<div class="runtime-row"><span class="runtime-label" style="opacity:0.4">Build + Run with flow on</span></div>';
      return;
    }

    const sorted = Array.from(executedLines.entries())
      .filter(([line, count]) => line > 0 && count > 1)
      .sort(([, a], [, b]) => b - a)
      .slice(0, 10);

    if (sorted.length === 0) {
      this.hotspotContainer.innerHTML = '<div class="runtime-row"><span class="runtime-label" style="opacity:0.4">No hot lines</span></div>';
      return;
    }

    const maxCount = sorted[0][1];
    this.hotspotContainer.innerHTML = '';

    for (const [line, count] of sorted) {
      const row = document.createElement('div');
      row.className = 'runtime-hotspot-row';
      row.style.cursor = 'pointer';
      row.addEventListener('click', () => {
        const editor = (window as any).__forgeEditor;
        if (editor) {
          editor.revealLineInCenter(line);
          editor.setPosition({ lineNumber: line, column: 1 });
        }
      });

      const lineLabel = document.createElement('span');
      lineLabel.className = 'runtime-hotspot-line';
      lineLabel.textContent = `L${line}`;
      row.appendChild(lineLabel);

      const barOuter = document.createElement('div');
      barOuter.className = 'runtime-hotspot-bar';
      const barInner = document.createElement('div');
      barInner.className = 'runtime-hotspot-fill';
      barInner.style.width = `${(count / maxCount) * 100}%`;
      barOuter.appendChild(barInner);
      row.appendChild(barOuter);

      const countLabel = document.createElement('span');
      countLabel.className = 'runtime-hotspot-count';
      countLabel.textContent = formatCount(count);
      row.appendChild(countLabel);

      this.hotspotContainer.appendChild(row);
    }
  }

  // ── History ──

  private updateHistory(): void {
    this.historyContainer.innerHTML = '';
    // Show last 10 runs, newest first
    const recent = this.runHistory.slice(-10).reverse();
    for (let i = 0; i < recent.length; i++) {
      const r = recent[i];
      const runIdx = this.runHistory.length - i;
      const row = document.createElement('div');
      row.className = 'runtime-row history-row';

      const label = document.createElement('span');
      label.className = 'runtime-label';
      label.textContent = `#${runIdx}`;
      row.appendChild(label);

      const value = document.createElement('span');
      value.className = 'runtime-value';
      value.textContent = `${formatDuration(r.duration)}  ${formatBytes(r.peakAllocation)}`;
      if (r.duration === this.personalBest) value.className = 'runtime-value accent';
      row.appendChild(value);

      // Save as benchmark button
      const saveBtn = document.createElement('span');
      saveBtn.className = 'history-save-btn';
      saveBtn.textContent = '⚑';
      saveBtn.title = 'Save as benchmark';
      saveBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        this.saveBenchmark(r, runIdx);
      });
      row.appendChild(saveBtn);

      this.historyContainer.appendChild(row);
    }

    // Also re-render benchmarks to update the "vs last run" delta
    this.renderBenchmarks();
  }

  // ── Helpers ──

  private makeSection(title: string): HTMLElement {
    const el = document.createElement('div');
    el.className = 'runtime-section-title';
    el.textContent = title;
    return el;
  }

  private addRow(parent: HTMLElement, label: string, initial: string): HTMLElement {
    const row = document.createElement('div');
    row.className = 'runtime-row';
    const labelEl = document.createElement('span');
    labelEl.className = 'runtime-label';
    labelEl.textContent = label;
    row.appendChild(labelEl);
    const valueEl = document.createElement('span');
    valueEl.className = 'runtime-value';
    valueEl.textContent = initial;
    row.appendChild(valueEl);
    parent.appendChild(row);
    return valueEl;
  }
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0B';
  if (bytes < 0) return '—';
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
}

function formatDuration(ms: number): string {
  if (ms < 1) return '<1ms';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60000) return `${(ms / 1000).toFixed(2)}s`;
  return `${Math.floor(ms / 60000)}m ${((ms % 60000) / 1000).toFixed(1)}s`;
}

function formatCount(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1000000) return `${(n / 1000).toFixed(1)}K`;
  return `${(n / 1000000).toFixed(1)}M`;
}
