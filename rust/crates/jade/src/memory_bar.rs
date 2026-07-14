//! Memory-bar model (feature inventory §5.3, port of `panels/memory-bar.ts`).
//!
//! The bottom strip shows SYS MEM · HEAP · PEAK · PRESSURE (5-dot gauge) on the
//! left and CPU% · GPU% on the right. HEAP/PEAK/leaks come from program memory
//! events ([`forge_build::MemoryEvent`]); SYS MEM and PRESSURE fall back to
//! OS-level [`forge_sysmon::SystemStats`] until (and, since we have no
//! program-level pressure signal, after) program data exists.
//!
//! Threshold classification is factored into pure, unit-tested functions so the
//! renderer only maps a [`Level`] to a color.

use forge_build::{AllocKind, MemoryEvent};
use forge_sysmon::SystemStats;

use crate::format::format_bytes;

/// Severity of a metric readout → color at paint time (normal/warn/danger map
/// to text / amber / red, `memory-bar.ts` `''`/`warn`/`danger`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Normal,
    Warn,
    Danger,
}

/// Heap-vs-peak classification (`memory-bar.ts:83-84`): danger >95%, warn >80%
/// of peak. NB the TS source has a check-order bug (`>0.8` shadows `>0.95`); the
/// Phase-3 brief pins the *intended* thresholds, so danger takes precedence.
pub fn heap_level(heap_used: i64, peak: i64) -> Level {
    if peak <= 0 {
        return Level::Normal;
    }
    let ratio = heap_used as f64 / peak as f64;
    if ratio > 0.95 {
        Level::Danger
    } else if ratio > 0.8 {
        Level::Warn
    } else {
        Level::Normal
    }
}

/// Peak is forced to danger whenever there are leaks (`memory-bar.ts:87-92`).
pub fn peak_level(leak_count: i64) -> Level {
    if leak_count > 0 {
        Level::Danger
    } else {
        Level::Normal
    }
}

/// Pressure gauge classification (`memory-bar.ts:104`): danger >80, warn >60.
pub fn pressure_level(pct: i64) -> Level {
    if pct > 80 {
        Level::Danger
    } else if pct > 60 {
        Level::Warn
    } else {
        Level::Normal
    }
}

/// CPU/GPU gauge classification (`memory-bar.ts:110,116`): danger >85, warn >60.
pub fn util_level(pct: i64) -> Level {
    if pct > 85 {
        Level::Danger
    } else if pct > 60 {
        Level::Warn
    } else {
        Level::Normal
    }
}

/// The 5-dot pressure gauge string: `filled = round(pct/20)`, filled `▪` /
/// empty `▫` (`memory-bar.ts:97-102`).
pub fn pressure_dots(pct: i64) -> String {
    let filled = ((pct as f64) / 20.0).round() as i64;
    (0..5)
        .map(|i| if i < filled { '▪' } else { '▫' })
        .collect()
}

/// Accumulated program-memory state, fed by [`MemoryEvent`]s. Mirrors the
/// renderer `MemoryBarData` fields the memory bar reads.
#[derive(Debug, Clone, Default)]
pub struct MemoryBarState {
    /// Current live heap bytes (`current_heap` from HeapSummary, or the running
    /// alloc−free total from AllocBatch between summaries).
    pub heap_used: i64,
    /// Peak live heap bytes ever observed.
    pub peak_allocation: i64,
    /// Leaked-allocation count (ASan leak summary); forces PEAK to danger.
    pub leak_count: i64,
    /// Total alloc events seen — used to decide "has program data yet".
    pub alloc_count: i64,
    /// Total free events seen (runtime panel MEMORY section, §5.4).
    pub free_count: i64,
    /// Per `file:line` cumulative allocated bytes (AllocBatch roll-up).
    pub per_line: std::collections::HashMap<String, i64>,
}

impl MemoryBarState {
    /// True once any program allocation data has arrived; before that the bar
    /// falls back to OS stats (`memory-bar.ts:128-129` `noRunData`).
    pub fn has_program_data(&self) -> bool {
        self.alloc_count > 0 || self.heap_used != 0
    }

    /// Reset at build/run start (`resetMemoryTracking`, app.ts:1024).
    pub fn reset(&mut self) {
        *self = MemoryBarState::default();
    }

    /// Fold a memory event into the accumulator. Returns the heap sample to feed
    /// [`crate::training::TrainingData::push_memory`] (so the Memory chart
    /// moves), if this event produced one.
    pub fn apply(&mut self, ev: &MemoryEvent) -> Option<f64> {
        match ev {
            MemoryEvent::AllocBatch { events } => {
                for e in events {
                    match e.kind {
                        AllocKind::Alloc => {
                            self.heap_used += e.size;
                            self.alloc_count += 1;
                            *self
                                .per_line
                                .entry(format!("{}:{}", e.file, e.line))
                                .or_insert(0) += e.size;
                        }
                        AllocKind::Free => {
                            self.heap_used -= e.size;
                            self.free_count += 1;
                        }
                    }
                    if self.heap_used > self.peak_allocation {
                        self.peak_allocation = self.heap_used;
                    }
                }
                Some(self.heap_used.max(0) as f64)
            }
            MemoryEvent::HeapSummary {
                current_heap,
                peak_heap,
                alloc_count,
                free_count,
                ..
            } => {
                // The interposer summary is authoritative for heap/peak.
                self.heap_used = *current_heap;
                if *peak_heap > self.peak_allocation {
                    self.peak_allocation = *peak_heap;
                }
                if *alloc_count > self.alloc_count {
                    self.alloc_count = *alloc_count;
                }
                if *free_count > self.free_count {
                    self.free_count = *free_count;
                }
                Some((*current_heap).max(0) as f64)
            }
            MemoryEvent::AsanLeakSummary {
                leaked_allocations, ..
            } => {
                self.leak_count = *leaked_allocations;
                None
            }
            _ => None,
        }
    }
}

/// A fully-resolved snapshot of what the memory bar should paint, combining
/// program state with the OS fallback. Pure projection — the renderer maps
/// each field's [`Level`] to a color.
pub struct MemoryBarView {
    pub sys_mem: String,
    pub heap: String,
    pub heap_level: Level,
    pub peak: String,
    pub peak_level: Level,
    pub pressure_dots: String,
    pub pressure_level: Level,
    pub cpu: String,
    pub cpu_level: Level,
    pub gpu: String,
    pub gpu_level: Level,
}

/// Project the memory bar. `mem` is program state; `sys` is the OS fallback for
/// SYS MEM + PRESSURE (§5.3 fallback rule).
pub fn project(mem: &MemoryBarState, sys: &SystemStats) -> MemoryBarView {
    // SYS MEM: OS used bytes (— until the first sysmon tick).
    let sys_mem = if sys.mem_total > 0 {
        format_bytes(sys.mem_used as f64)
    } else {
        "—".to_string()
    };

    // PRESSURE: OS memory pressure (the only pressure signal we have).
    let pressure_pct = if sys.mem_total > 0 {
        ((sys.mem_used as f64 / sys.mem_total as f64) * 100.0).round() as i64
    } else {
        0
    };

    // HEAP / PEAK: program data, or — before any run.
    let (heap, heap_level_v, peak, peak_level_v) = if mem.has_program_data() {
        let heap = format_bytes(mem.heap_used.max(0) as f64);
        let hl = heap_level(mem.heap_used, mem.peak_allocation);
        let peak = if mem.leak_count > 0 {
            format!(
                "{} ({} leaked)",
                format_bytes(mem.peak_allocation as f64),
                mem.leak_count
            )
        } else {
            format_bytes(mem.peak_allocation as f64)
        };
        (heap, hl, peak, peak_level(mem.leak_count))
    } else {
        (
            "—".to_string(),
            Level::Normal,
            "—".to_string(),
            Level::Normal,
        )
    };

    let cpu = format!("{}%", sys.cpu_percent);
    let (gpu, gpu_level_v) = if sys.gpu_percent >= 0 {
        (format!("{}%", sys.gpu_percent), util_level(sys.gpu_percent))
    } else {
        ("—".to_string(), Level::Normal)
    };

    MemoryBarView {
        sys_mem,
        heap,
        heap_level: heap_level_v,
        peak,
        peak_level: peak_level_v,
        pressure_dots: pressure_dots(pressure_pct),
        pressure_level: pressure_level(pressure_pct),
        cpu,
        cpu_level: util_level(sys.cpu_percent),
        gpu,
        gpu_level: gpu_level_v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_build::AllocEvent;

    fn alloc(size: i64, file: &str, line: u32) -> AllocEvent {
        AllocEvent {
            kind: AllocKind::Alloc,
            pointer: "0x1".into(),
            size,
            file: file.into(),
            line,
            timestamp: 0.0,
        }
    }
    fn free(size: i64) -> AllocEvent {
        AllocEvent {
            kind: AllocKind::Free,
            pointer: "0x1".into(),
            size,
            file: "".into(),
            line: 0,
            timestamp: 0.0,
        }
    }

    #[test]
    fn heap_thresholds() {
        assert_eq!(heap_level(50, 100), Level::Normal); // 50%
        assert_eq!(heap_level(85, 100), Level::Warn); // 85%
        assert_eq!(heap_level(96, 100), Level::Danger); // 96% — danger wins
        assert_eq!(heap_level(100, 0), Level::Normal); // no peak yet
    }

    #[test]
    fn pressure_and_util_thresholds() {
        assert_eq!(pressure_level(50), Level::Normal);
        assert_eq!(pressure_level(70), Level::Warn);
        assert_eq!(pressure_level(90), Level::Danger);
        assert_eq!(util_level(60), Level::Normal); // >60 is warn, 60 is normal
        assert_eq!(util_level(61), Level::Warn);
        assert_eq!(util_level(86), Level::Danger);
    }

    #[test]
    fn peak_forced_danger_on_leak() {
        assert_eq!(peak_level(0), Level::Normal);
        assert_eq!(peak_level(3), Level::Danger);
    }

    #[test]
    fn dots_gauge() {
        assert_eq!(pressure_dots(0), "▫▫▫▫▫");
        assert_eq!(pressure_dots(50), "▪▪▪▫▫"); // round(2.5)=3 (half away from zero, matches Math.round)
        assert_eq!(pressure_dots(40), "▪▪▫▫▫"); // round(2.0)=2
        assert_eq!(pressure_dots(100), "▪▪▪▪▪");
    }

    #[test]
    fn alloc_batch_accumulates_heap_and_peak() {
        let mut m = MemoryBarState::default();
        let sample = m.apply(&MemoryEvent::AllocBatch {
            events: vec![alloc(1000, "main.cpp", 10), alloc(500, "main.cpp", 10), free(500)],
        });
        assert_eq!(m.heap_used, 1000);
        assert_eq!(m.peak_allocation, 1500); // peaked at 1500 before the free
        assert_eq!(m.alloc_count, 2);
        assert_eq!(*m.per_line.get("main.cpp:10").unwrap(), 1500);
        assert_eq!(sample, Some(1000.0));
    }

    #[test]
    fn heap_summary_is_authoritative() {
        let mut m = MemoryBarState::default();
        m.apply(&MemoryEvent::HeapSummary {
            total_alloc: 5000,
            total_freed: 1000,
            current_heap: 4000,
            peak_heap: 6000,
            alloc_count: 12,
            free_count: 3,
        });
        assert_eq!(m.heap_used, 4000);
        assert_eq!(m.peak_allocation, 6000);
        assert!(m.has_program_data());
    }

    #[test]
    fn fallback_before_program_data() {
        let mem = MemoryBarState::default();
        let sys = SystemStats {
            cpu_percent: 10,
            gpu_percent: -1,
            gpu_name: "x".into(),
            mem_total: 100,
            mem_used: 70,
        };
        let v = project(&mem, &sys);
        assert_eq!(v.heap, "—");
        assert_eq!(v.peak, "—");
        assert_eq!(v.gpu, "—"); // -1 → unavailable
        assert_eq!(v.pressure_level, Level::Warn); // 70% OS pressure
        assert_ne!(v.sys_mem, "—"); // SYS MEM from OS fallback
    }

    #[test]
    fn leak_annotation_in_view() {
        let mut mem = MemoryBarState::default();
        mem.apply(&MemoryEvent::HeapSummary {
            total_alloc: 100,
            total_freed: 0,
            current_heap: 100,
            peak_heap: 100,
            alloc_count: 1,
            free_count: 0,
        });
        mem.apply(&MemoryEvent::AsanLeakSummary {
            leaked_bytes: 100,
            leaked_allocations: 2,
        });
        let sys = SystemStats::default();
        let v = project(&mem, &sys);
        assert!(v.peak.contains("(2 leaked)"));
        assert_eq!(v.peak_level, Level::Danger);
    }
}
