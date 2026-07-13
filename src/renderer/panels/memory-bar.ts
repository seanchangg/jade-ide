import { store } from '../state';
import type { MemoryBarData, SystemStats } from '../../shared/types';

export class MemoryBar {
  private container: HTMLElement;
  private heapEl: HTMLElement;
  private stackEl: HTMLElement;
  private peakEl: HTMLElement;
  private pressureEl: HTMLElement;
  private pressureDots: HTMLElement;
  private cpuEl: HTMLElement;
  private gpuEl: HTMLElement;

  constructor(parent: HTMLElement) {
    this.container = document.createElement('div');
    this.container.className = 'memory-bar';

    // Left section — memory metrics
    const memSection = document.createElement('div');
    memSection.className = 'memory-bar-section';

    this.heapEl = this.createMetric(memSection, 'SYS MEM', '—');
    this.stackEl = this.createMetric(memSection, 'HEAP', '—');
    this.peakEl = this.createMetric(memSection, 'PEAK', '—');
    this.pressureEl = this.createMetric(memSection, 'PRESSURE', '');

    this.pressureDots = document.createElement('span');
    this.pressureDots.className = 'memory-bar-dots';
    this.pressureDots.textContent = '▪▪▪▪▪';
    this.pressureEl.appendChild(this.pressureDots);

    this.container.appendChild(memSection);

    // Right section — system gauges
    const sysSection = document.createElement('div');
    sysSection.className = 'memory-bar-section memory-bar-right';

    this.cpuEl = this.createMetric(sysSection, 'CPU', '—');
    this.gpuEl = this.createMetric(sysSection, 'GPU', '—');

    this.container.appendChild(sysSection);

    parent.appendChild(this.container);

    // Subscribe to updates
    store.on('memoryBar', (data: MemoryBarData) => this.updateMemory(data));
    store.on('systemStats', (stats: SystemStats) => this.updateSystem(stats));

    store.on('memoryBarVisible', (visible: boolean) => {
      this.container.classList.toggle('visible', visible);
    });

    // Initial visibility
    if (store.get<boolean>('memoryBarVisible')) {
      this.container.classList.add('visible');
    }
  }

  private createMetric(parent: HTMLElement, label: string, initial: string): HTMLElement {
    const metric = document.createElement('div');
    metric.className = 'memory-bar-metric';

    const labelEl = document.createElement('span');
    labelEl.className = 'memory-bar-label';
    labelEl.textContent = label;
    metric.appendChild(labelEl);

    const valueEl = document.createElement('span');
    valueEl.className = 'memory-bar-value';
    valueEl.textContent = initial;
    metric.appendChild(valueEl);

    parent.appendChild(metric);
    return valueEl;
  }

  private updateMemory(data: MemoryBarData): void {
    this.heapEl.textContent = formatBytes(data.heapUsed);
    this.stackEl.textContent = formatBytes(data.stackSize);
    this.peakEl.textContent = formatBytes(data.peakAllocation);

    // Color heap red if it's high relative to peak
    const ratio = data.peakAllocation > 0 ? data.heapUsed / data.peakAllocation : 0;
    this.heapEl.className = `memory-bar-value ${ratio > 0.8 ? 'warn' : ratio > 0.95 ? 'danger' : ''}`;

    // Leak indicator
    if (data.leakCount > 0) {
      this.peakEl.className = 'memory-bar-value danger';
      this.peakEl.textContent = `${formatBytes(data.peakAllocation)} (${data.leakCount} leaked)`;
    } else {
      this.peakEl.className = 'memory-bar-value';
    }

    // Pressure dots
    const pct = data.pressurePercent;
    const filled = Math.round(pct / 20); // 5 dots
    let dots = '';
    for (let i = 0; i < 5; i++) {
      dots += i < filled ? '▪' : '▫';
    }
    this.pressureDots.textContent = dots;
    this.pressureDots.className = `memory-bar-dots ${pct > 80 ? 'danger' : pct > 60 ? 'warn' : ''}`;
  }

  private updateSystem(stats: SystemStats): void {
    // CPU
    this.cpuEl.textContent = `${stats.cpuPercent}%`;
    this.cpuEl.className = `memory-bar-value ${stats.cpuPercent > 85 ? 'danger' : stats.cpuPercent > 60 ? 'warn' : ''}`;

    // GPU
    if (stats.gpuPercent >= 0) {
      this.gpuEl.textContent = `${stats.gpuPercent}%`;
      this.gpuEl.className = `memory-bar-value ${stats.gpuPercent > 85 ? 'danger' : stats.gpuPercent > 60 ? 'warn' : ''}`;
    } else {
      this.gpuEl.textContent = '—';
      this.gpuEl.className = 'memory-bar-value muted';
    }

    // System memory — only update SYS MEM field; leave HEAP/PEAK for program data
    if (stats.memTotal > 0) {
      this.heapEl.textContent = formatBytes(stats.memUsed);

      // Pressure from system memory when no program has run
      const memBar = store.get<MemoryBarData>('memoryBar');
      const noRunData = !memBar || (memBar.allocCount === 0 && memBar.heapUsed === 0);
      if (noRunData) {
        const pct = Math.round((stats.memUsed / stats.memTotal) * 100);
        let dots = '';
        for (let i = 0; i < 5; i++) {
          dots += i < Math.round(pct / 20) ? '▪' : '▫';
        }
        this.pressureDots.textContent = dots;
        this.pressureDots.className = `memory-bar-dots ${pct > 80 ? 'danger' : pct > 60 ? 'warn' : ''}`;
      }
    }
  }
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0B';
  if (bytes < 0) return '—';
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)}GB`;
}
