import { store } from '../state';
import type { SystemStats } from '../../shared/types';

const forge = (window as any).forge;

// System gauges are rendered inside the memory bar.
// This module handles starting/stopping the monitor and
// computing smoothed values to avoid jittery display.

let smoothedCpu = 0;
let smoothedGpu = 0;
const SMOOTHING = 0.3; // exponential moving average weight

export function initSystemGauges(): void {
  forge.system.onStats((stats: SystemStats) => {
    // Smooth the values
    smoothedCpu = smoothedCpu * (1 - SMOOTHING) + stats.cpuPercent * SMOOTHING;
    smoothedGpu = stats.gpuPercent >= 0
      ? smoothedGpu * (1 - SMOOTHING) + stats.gpuPercent * SMOOTHING
      : -1;

    store.set('systemStats', {
      cpuPercent: Math.round(smoothedCpu),
      gpuPercent: smoothedGpu >= 0 ? Math.round(smoothedGpu) : -1,
      gpuName: stats.gpuName,
      memTotal: stats.memTotal,
      memUsed: stats.memUsed,
    });
  });

  // Start monitoring
  forge.system.start();
}

export function stopSystemGauges(): void {
  forge.system.stop();
}
