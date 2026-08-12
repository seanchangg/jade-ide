//! Jade system monitor — Rust port of `src/main/system-monitor.ts`.
//!
//! Polls CPU% and memory every 1500ms and, best-effort, GPU utilization every
//! 4th tick (~6s), publishing a [`SystemStats`] snapshot over a
//! [`tokio::sync::watch`] channel (the analogue of the TS `onStats` callback,
//! TS :16-36). See `docs/jade-feature-inventory.md` §8.6.
//!
//! Deviations from the TS, all deliberate:
//! - CPU% comes from `sysinfo`'s global usage (which itself tracks per-core
//!   idle/total deltas between refreshes) instead of hand-rolling the
//!   `os.cpus()` delta math (TS :45-54, :89-100). The inventory (§8.6) marks
//!   this `[COMMODITY]` and the task brief picks `sysinfo` for simplicity.
//! - GPU utilization is read from `ioreg -r -d 1 -w0 -c IOAccelerator` and the
//!   `"Device Utilization %"` key — the same source the Electron app's
//!   `systeminformation` used on macOS (TS :62-77). `-1` / `"unavailable"` when
//!   unreadable, cached between the expensive polls.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sysinfo::System;
use tokio::sync::watch;
use tokio::task::JoinHandle;

mod gpu;
pub use gpu::parse_ioreg_utilization;

/// A system-stats snapshot — port of `SystemStats` (shared/types.ts :133-139).
/// `gpu_percent` is `-1` when GPU data is unavailable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStats {
    /// Whole-percent CPU utilization, 0-100 (TS :50-52).
    pub cpu_percent: i64,
    /// Whole-percent GPU utilization, or `-1` if unavailable (TS :135).
    pub gpu_percent: i64,
    pub gpu_name: String,
    /// Total physical memory, bytes (TS :57).
    pub mem_total: u64,
    /// Used physical memory, bytes (`totalmem - freemem`, TS :58).
    pub mem_used: u64,
}

impl Default for SystemStats {
    fn default() -> Self {
        // Matches the TS seed values before the first GPU poll (TS :12-13).
        SystemStats {
            cpu_percent: 0,
            gpu_percent: -1,
            gpu_name: "unavailable".to_string(),
            mem_total: 0,
            mem_used: 0,
        }
    }
}

/// GPU re-poll cadence: every 4th tick, ~6s at the 1.5s interval (TS :14).
const GPU_POLL_EVERY: u64 = 4;
/// Poll interval (TS :35, :602).
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// The system monitor. Holds a broadcast [`watch`] channel so `start()` is
/// idempotent and multiple UI consumers can subscribe.
pub struct SystemMonitor {
    tx: watch::Sender<SystemStats>,
    task: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl SystemMonitor {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(SystemStats::default());
        SystemMonitor {
            tx,
            task: std::sync::Mutex::new(None),
        }
    }

    /// Subscribe to stats updates; the receiver's initial value is the latest
    /// published snapshot (default until the first tick).
    pub fn subscribe(&self) -> watch::Receiver<SystemStats> {
        self.tx.subscribe()
    }

    /// Start polling at the production 1500ms interval. Idempotent (TS :17).
    pub fn start(&self) {
        self.start_with_interval(POLL_INTERVAL);
    }

    /// Start polling at a custom interval. Exposed for tests that need a short
    /// interval; production code uses [`start`](Self::start). Idempotent.
    pub fn start_with_interval(&self, interval: Duration) {
        let mut task = self.task.lock().unwrap();
        if task.is_some() {
            return; // already running (TS :17 `if (this.timer) return`)
        }
        let tx = self.tx.clone();
        *task = Some(tokio::spawn(poll_loop(tx, interval)));
    }

    /// Stop polling and clear the timer (TS :38-43). Idempotent.
    pub fn stop(&self) {
        if let Some(handle) = self.task.lock().unwrap().take() {
            handle.abort();
        }
    }
}

impl Default for SystemMonitor {
    fn default() -> Self {
        Self::new()
    }
}

async fn poll_loop(tx: watch::Sender<SystemStats>, interval: Duration) {
    let mut sys = System::new();
    // Seed CPU values — the first usage reading needs a prior refresh (TS :26-29).
    sys.refresh_cpu_usage();

    let mut tick_count: u64 = 0;
    let mut cached_gpu_percent: i64 = -1;
    let mut cached_gpu_name = String::from("unavailable");

    loop {
        tokio::time::sleep(interval).await;

        // CPU (TS :45-54). sysinfo needs the interval between refreshes to be
        // >= MINIMUM_CPU_UPDATE_INTERVAL (~200ms); the 1.5s poll clears that.
        sys.refresh_cpu_usage();
        let cpu_percent = sys.global_cpu_usage().round() as i64;

        // Memory (TS :57-58). sysinfo reports bytes.
        sys.refresh_memory();
        let mem_total = sys.total_memory();
        let mem_used = sys.used_memory();

        // GPU — best-effort, only every 4th tick; reuse cache in between (TS :60-78).
        let should_poll_gpu = tick_count.is_multiple_of(GPU_POLL_EVERY);
        tick_count = tick_count.wrapping_add(1);
        if should_poll_gpu {
            if let Some((pct, name)) = gpu::sample_gpu().await {
                cached_gpu_percent = pct;
                cached_gpu_name = name;
            }
            // On failure, leave the cached value as-is (TS :75-77).
        }

        let stats = SystemStats {
            cpu_percent,
            gpu_percent: cached_gpu_percent,
            gpu_name: cached_gpu_name.clone(),
            mem_total,
            mem_used,
        };

        // No active receivers is not an error — keep the latest value.
        let _ = tx.send_replace(stats);
    }
}
