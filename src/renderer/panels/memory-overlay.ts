import { store } from '../state';
import type { MemoryBarData } from '../../shared/types';

// Floating overlay widget — shows memory + runtime stats.
// Tracks run history for PB, delta, and per-run comparisons.

let overlay: HTMLElement | null = null;
let dismissed = false;

// Run history
interface RunRecord {
  duration: number;
  heapUsed: number;
  peakAllocation: number;
  allocCount: number;
  timestamp: number;
}

const runHistory: RunRecord[] = [];
let personalBest: number = Infinity;

// Row element references
let heapRow: RowElements;
let peakRow: RowElements;
let allocsRow: RowElements;
let freesRow: RowElements;
let leaksRow: RowElements;
let durationRow: RowElements;
let pbRow: RowElements;
let deltaRow: RowElements;
let hotspotContainer: HTMLElement;

export function initMemoryOverlay(): void {
  overlay = document.createElement('div');
  overlay.className = 'memory-overlay';
  overlay.style.display = 'none';
  document.body.appendChild(overlay);

  // Header
  const header = document.createElement('div');
  header.className = 'memory-overlay-header';
  const title = document.createElement('span');
  title.className = 'memory-overlay-title';
  title.textContent = 'RUNTIME';
  header.appendChild(title);
  const closeBtn = document.createElement('span');
  closeBtn.className = 'memory-overlay-close';
  closeBtn.textContent = '\u00d7';
  closeBtn.addEventListener('click', () => {
    dismissed = true;
    if (overlay) overlay.style.display = 'none';
  });
  header.appendChild(closeBtn);
  overlay.appendChild(header);

  // Speed section
  const speedSection = document.createElement('div');
  speedSection.className = 'memory-overlay-section';
  const speedTitle = document.createElement('div');
  speedTitle.className = 'memory-overlay-section-title';
  speedTitle.textContent = 'SPEED';
  speedSection.appendChild(speedTitle);
  const speedRows = document.createElement('div');
  speedRows.className = 'memory-overlay-rows';
  durationRow = createRow(speedRows, 'Duration', '—');
  pbRow = createRow(speedRows, 'Best', '—');
  deltaRow = createRow(speedRows, 'vs Last', '—');
  speedSection.appendChild(speedRows);
  overlay.appendChild(speedSection);

  // Memory section
  const memSection = document.createElement('div');
  memSection.className = 'memory-overlay-section';
  const memTitle = document.createElement('div');
  memTitle.className = 'memory-overlay-section-title';
  memTitle.textContent = 'MEMORY';
  memSection.appendChild(memTitle);
  const memRows = document.createElement('div');
  memRows.className = 'memory-overlay-rows';
  heapRow = createRow(memRows, 'Heap', '—');
  peakRow = createRow(memRows, 'Peak', '—');
  allocsRow = createRow(memRows, 'Allocs', '—');
  freesRow = createRow(memRows, 'Frees', '—');
  leaksRow = createRow(memRows, 'Leaks', '—');
  memSection.appendChild(memRows);
  overlay.appendChild(memSection);

  // Hotspot section (top executed lines from gcov)
  const hotSection = document.createElement('div');
  hotSection.className = 'memory-overlay-section';
  const hotTitle = document.createElement('div');
  hotTitle.className = 'memory-overlay-section-title';
  hotTitle.textContent = 'HOTSPOTS';
  hotSection.appendChild(hotTitle);
  hotspotContainer = document.createElement('div');
  hotspotContainer.className = 'memory-overlay-rows';
  hotSection.appendChild(hotspotContainer);
  overlay.appendChild(hotSection);

  // Subscribe to memory data
  store.on('memoryBar', (data: MemoryBarData) => {
    heapRow.value.textContent = formatBytes(data.heapUsed);
    peakRow.value.textContent = formatBytes(data.peakAllocation);
    allocsRow.value.textContent = String(data.allocCount);
    freesRow.value.textContent = String(data.freeCount);
    leaksRow.value.textContent = String(data.leakCount);
    leaksRow.value.className = data.leakCount > 0
      ? 'memory-overlay-value error' : 'memory-overlay-value';
    updateVisibility(data);
  });

  // Subscribe to run state
  store.on('isRunning', () => {
    const running = store.get<boolean>('isRunning');
    if (running) {
      dismissed = false;
      durationRow.value.textContent = '...';
      durationRow.value.className = 'memory-overlay-value';
    }
    updateVisibility(store.get<MemoryBarData>('memoryBar'));
  });

  // Subscribe to executed lines for hotspot display
  store.on('executedLines', () => {
    updateHotspots();
  });
}

// Called from app.ts after a run completes with the duration
export function recordRun(duration: number): void {
  const memBar = store.get<MemoryBarData>('memoryBar');

  const record: RunRecord = {
    duration,
    heapUsed: memBar?.heapUsed || 0,
    peakAllocation: memBar?.peakAllocation || 0,
    allocCount: memBar?.allocCount || 0,
    timestamp: Date.now(),
  };

  // Update personal best
  if (duration < personalBest) {
    personalBest = duration;
  }

  // Update duration display
  durationRow.value.textContent = formatDuration(duration);

  // Personal best
  pbRow.value.textContent = formatDuration(personalBest);
  if (duration === personalBest) {
    pbRow.value.className = 'memory-overlay-value accent';
  } else {
    pbRow.value.className = 'memory-overlay-value';
  }

  // Delta vs last run
  if (runHistory.length > 0) {
    const last = runHistory[runHistory.length - 1];
    const diff = duration - last.duration;
    const pct = last.duration > 0 ? ((diff / last.duration) * 100) : 0;
    if (diff < 0) {
      deltaRow.value.textContent = `${formatDuration(Math.abs(diff))} faster (${Math.abs(pct).toFixed(1)}%)`;
      deltaRow.value.className = 'memory-overlay-value accent';
    } else if (diff > 0) {
      deltaRow.value.textContent = `${formatDuration(diff)} slower (${pct.toFixed(1)}%)`;
      deltaRow.value.className = 'memory-overlay-value error';
    } else {
      deltaRow.value.textContent = 'same';
      deltaRow.value.className = 'memory-overlay-value';
    }
  } else {
    deltaRow.value.textContent = '—';
    deltaRow.value.className = 'memory-overlay-value';
  }

  runHistory.push(record);
}

function updateHotspots(): void {
  const executedLines = store.get<Map<number, number>>('executedLines');
  if (!executedLines || executedLines.size === 0) {
    hotspotContainer.innerHTML = '<div class="memory-overlay-row"><span class="memory-overlay-label" style="opacity:0.5">Run with flow arrows on</span></div>';
    return;
  }

  // Sort by count descending, take top 5
  const sorted = Array.from(executedLines.entries())
    .filter(([line, count]) => line > 0 && count > 1)
    .sort(([, a], [, b]) => b - a)
    .slice(0, 5);

  if (sorted.length === 0) {
    hotspotContainer.innerHTML = '<div class="memory-overlay-row"><span class="memory-overlay-label" style="opacity:0.5">No hot lines</span></div>';
    return;
  }

  const maxCount = sorted[0][1];
  hotspotContainer.innerHTML = '';

  for (const [line, count] of sorted) {
    const row = document.createElement('div');
    row.className = 'memory-overlay-hotspot-row';

    const lineLabel = document.createElement('span');
    lineLabel.className = 'memory-overlay-hotspot-line';
    lineLabel.textContent = `L${line}`;
    row.appendChild(lineLabel);

    const barOuter = document.createElement('div');
    barOuter.className = 'memory-overlay-hotspot-bar';
    const barInner = document.createElement('div');
    barInner.className = 'memory-overlay-hotspot-bar-fill';
    barInner.style.width = `${(count / maxCount) * 100}%`;
    barOuter.appendChild(barInner);
    row.appendChild(barOuter);

    const countLabel = document.createElement('span');
    countLabel.className = 'memory-overlay-hotspot-count';
    countLabel.textContent = formatCount(count);
    row.appendChild(countLabel);

    // Click to jump to line
    row.style.cursor = 'pointer';
    row.addEventListener('click', () => {
      const editor = (window as any).__forgeEditor;
      if (editor) {
        editor.revealLineInCenter(line);
        editor.setPosition({ lineNumber: line, column: 1 });
      }
    });

    hotspotContainer.appendChild(row);
  }
}

function updateVisibility(data: MemoryBarData | undefined): void {
  if (!overlay || dismissed) return;
  const isRunning = store.get<boolean>('isRunning');
  const hasData = data !== undefined && (data.allocCount > 0 || data.heapUsed > 0 || data.peakAllocation > 0);
  const hasRunHistory = runHistory.length > 0;
  if (isRunning || hasData || hasRunHistory) {
    overlay.style.display = 'block';
  }
}

interface RowElements { label: HTMLElement; value: HTMLElement; }

function createRow(parent: HTMLElement, labelText: string, initialValue: string): RowElements {
  const row = document.createElement('div');
  row.className = 'memory-overlay-row';
  const label = document.createElement('span');
  label.className = 'memory-overlay-label';
  label.textContent = labelText;
  row.appendChild(label);
  const value = document.createElement('span');
  value.className = 'memory-overlay-value';
  value.textContent = initialValue;
  row.appendChild(value);
  parent.appendChild(row);
  return { label, value };
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0B';
  if (bytes < 0) return '\u2014';
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)}GB`;
}

function formatDuration(ms: number): string {
  if (ms < 1) return '<1ms';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60000) return `${(ms / 1000).toFixed(2)}s`;
  const mins = Math.floor(ms / 60000);
  const secs = ((ms % 60000) / 1000).toFixed(1);
  return `${mins}m ${secs}s`;
}

function formatCount(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1000000) return `${(n / 1000).toFixed(1)}K`;
  return `${(n / 1000000).toFixed(1)}M`;
}
