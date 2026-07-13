import * as os from 'os';
import { SystemStats } from '../shared/types';

export class SystemMonitor {
  private timer: ReturnType<typeof setInterval> | null = null;
  private prevCpuIdle = 0;
  private prevCpuTotal = 0;
  private si: any = null; // systeminformation, loaded lazily
  // GPU polling is expensive (si.graphics() enumerates controllers each call).
  // CPU/mem is cheap and polled every tick; GPU is cached and refreshed less often.
  private tickCount = 0;
  private cachedGpuPercent = -1;
  private cachedGpuName = 'unavailable';
  private static readonly GPU_POLL_EVERY = 4; // ~every 6s at a 1.5s interval

  start(onStats: (stats: SystemStats) => void): void {
    if (this.timer) return;

    // Try to load systeminformation for GPU data
    try {
      this.si = require('systeminformation');
    } catch {
      // GPU monitoring unavailable
    }

    // Seed CPU values
    const cpuInfo = this.getCpuSnapshot();
    this.prevCpuIdle = cpuInfo.idle;
    this.prevCpuTotal = cpuInfo.total;

    // Poll every 1.5 seconds — frequent enough to feel live, cheap enough to be invisible
    this.timer = setInterval(async () => {
      const stats = await this.collect();
      onStats(stats);
    }, 1500);
  }

  stop(): void {
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  private async collect(): Promise<SystemStats> {
    // CPU utilization via delta of idle time
    const cpuInfo = this.getCpuSnapshot();
    const idleDelta = cpuInfo.idle - this.prevCpuIdle;
    const totalDelta = cpuInfo.total - this.prevCpuTotal;
    const cpuPercent = totalDelta > 0
      ? Math.round((1 - idleDelta / totalDelta) * 100)
      : 0;
    this.prevCpuIdle = cpuInfo.idle;
    this.prevCpuTotal = cpuInfo.total;

    // Memory
    const memTotal = os.totalmem();
    const memUsed = memTotal - os.freemem();

    // GPU — best effort. Only re-query occasionally; reuse the cached value on
    // intermediate ticks so the expensive graphics() call doesn't run every 1.5s.
    const shouldPollGpu = this.si && (this.tickCount % SystemMonitor.GPU_POLL_EVERY === 0);
    this.tickCount++;

    if (shouldPollGpu) {
      try {
        const graphics = await this.si.graphics();
        if (graphics.controllers && graphics.controllers.length > 0) {
          const gpu = graphics.controllers[0];
          this.cachedGpuName = gpu.model || 'unknown';
          // systeminformation may return utilizationGpu on some systems
          this.cachedGpuPercent = typeof gpu.utilizationGpu === 'number'
            ? gpu.utilizationGpu : -1;
        }
      } catch {
        // GPU query failed — leave cached value as-is
      }
    }

    return {
      cpuPercent,
      gpuPercent: this.cachedGpuPercent,
      gpuName: this.cachedGpuName,
      memTotal,
      memUsed,
    };
  }

  private getCpuSnapshot(): { idle: number; total: number } {
    const cpus = os.cpus();
    let idle = 0;
    let total = 0;

    for (const cpu of cpus) {
      idle += cpu.times.idle;
      total += cpu.times.user + cpu.times.nice + cpu.times.sys + cpu.times.irq + cpu.times.idle;
    }

    return { idle, total };
  }
}
