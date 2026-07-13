//! Training data buffers (feature inventory §7.1, §10). Pure state feeding the
//! Loss / Memory / Kernel-time / Timing / Tensor visualizations.
//!
//! Caps, matching the TS `TrainingView`:
//!   • scalars & memory: last **1000** points
//!   • tensors: ring of **32** frames per buffer
//!   • timings: **5000** total, eviction preserving the **last 500 per name**
//!
//! On `clear()` the current run is snapshotted as the *ghost* previous run, so
//! charts can underlay it at reduced alpha.

use std::collections::{HashMap, VecDeque};

pub const MAX_POINTS: usize = 1000;
pub const MAX_TENSOR_FRAMES: usize = 32;
pub const MAX_TIMINGS: usize = 5000;
pub const MAX_TIMINGS_PER_NAME: usize = 500;

#[derive(Debug, Clone, Copy)]
pub struct ScalarPoint {
    pub step: i64,
    pub value: f64,
}

#[derive(Debug, Clone)]
pub struct TimingPoint {
    pub name: String,
    pub ms: f64,
    pub step: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct MemPoint {
    pub heap_used: f64,
}

#[derive(Debug, Clone)]
pub struct TensorFrame {
    pub step: i64,
    pub rows: u32,
    pub cols: u32,
    pub src_rows: Option<u32>,
    pub src_cols: Option<u32>,
    pub data: Vec<f32>,
}

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub min: f64,
    pub max: f64,
}

/// One run's worth of series data (current or ghost-previous).
#[derive(Debug, Default, Clone)]
pub struct RunData {
    pub scalars: HashMap<String, Vec<ScalarPoint>>,
    pub scalar_stats: HashMap<String, Stats>,
    pub timings: Vec<TimingPoint>,
    pub memory: Vec<MemPoint>,
    pub memory_max: f64,
}

#[derive(Debug, Default)]
pub struct TrainingData {
    pub current: RunData,
    /// Snapshot of the previous run for the ghost overlay.
    pub previous: RunData,
    /// Ring buffers of the latest tensor frames per enabled buffer.
    pub tensors: HashMap<String, VecDeque<TensorFrame>>,
}

impl TrainingData {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_scalar(&mut self, name: &str, step: i64, value: f64) {
        let arr = self.current.scalars.entry(name.to_string()).or_default();
        arr.push(ScalarPoint { step, value });
        if arr.len() > MAX_POINTS {
            let drop = arr.len() - MAX_POINTS;
            arr.drain(0..drop);
            recompute_stats(&mut self.current.scalar_stats, name, arr);
        } else {
            let st = self
                .current
                .scalar_stats
                .entry(name.to_string())
                .or_insert(Stats {
                    min: f64::INFINITY,
                    max: f64::NEG_INFINITY,
                });
            if value < st.min {
                st.min = value;
            }
            if value > st.max {
                st.max = value;
            }
        }
    }

    pub fn push_timing(&mut self, name: &str, ms: f64, step: i64) {
        self.current.timings.push(TimingPoint {
            name: name.to_string(),
            ms,
            step,
        });
        if self.current.timings.len() > MAX_TIMINGS {
            evict_timings(&mut self.current.timings);
        }
    }

    pub fn push_memory(&mut self, heap_used: f64) {
        if heap_used <= 0.0 {
            return;
        }
        self.current.memory.push(MemPoint { heap_used });
        if self.current.memory.len() > MAX_POINTS {
            let drop = self.current.memory.len() - MAX_POINTS;
            self.current.memory.drain(0..drop);
            self.current.memory_max = self
                .current
                .memory
                .iter()
                .map(|m| m.heap_used)
                .fold(0.0, f64::max);
        } else if heap_used > self.current.memory_max {
            self.current.memory_max = heap_used;
        }
    }

    pub fn push_tensor(&mut self, name: &str, frame: TensorFrame) {
        let ring = self.tensors.entry(name.to_string()).or_default();
        ring.push_back(frame);
        while ring.len() > MAX_TENSOR_FRAMES {
            ring.pop_front();
        }
    }

    pub fn latest_tensor(&self, name: &str) -> Option<&TensorFrame> {
        self.tensors.get(name).and_then(|r| r.back())
    }

    /// Snapshot the current run as the ghost previous run, then reset current
    /// (feature inventory §7.1 ghost-overlay-on-clear).
    pub fn clear(&mut self) {
        let has_data = !self.current.scalars.is_empty()
            || !self.current.timings.is_empty()
            || !self.current.memory.is_empty();
        if has_data {
            self.previous = std::mem::take(&mut self.current);
        } else {
            self.current = RunData::default();
        }
        self.tensors.clear();
    }
}

fn recompute_stats(stats: &mut HashMap<String, Stats>, name: &str, arr: &[ScalarPoint]) {
    if arr.is_empty() {
        stats.remove(name);
        return;
    }
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for p in arr {
        if p.value < min {
            min = p.value;
        }
        if p.value > max {
            max = p.value;
        }
    }
    stats.insert(name.to_string(), Stats { min, max });
}

/// Trim to `MAX_TIMINGS` while keeping at most `MAX_TIMINGS_PER_NAME` of the
/// most recent samples for each timer name (so one busy timer can't evict
/// another's history) — training-view.ts:233-244.
fn evict_timings(timings: &mut Vec<TimingPoint>) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut keep = vec![true; timings.len()];
    for i in (0..timings.len()).rev() {
        let name = timings[i].name.as_str();
        let c = counts.entry(name).or_insert(0);
        *c += 1;
        if *c > MAX_TIMINGS_PER_NAME {
            keep[i] = false;
        }
    }
    let mut idx = 0;
    timings.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_cap_at_max_points() {
        let mut d = TrainingData::new();
        for i in 0..(MAX_POINTS + 250) {
            d.push_scalar("loss", i as i64, i as f64);
        }
        let arr = d.current.scalars.get("loss").unwrap();
        assert_eq!(arr.len(), MAX_POINTS);
        // oldest dropped: first retained value is 250
        assert_eq!(arr[0].value, 250.0);
        let st = d.current.scalar_stats.get("loss").unwrap();
        assert_eq!(st.min, 250.0);
    }

    #[test]
    fn tensor_ring_holds_last_32() {
        let mut d = TrainingData::new();
        for i in 0..50 {
            d.push_tensor(
                "W",
                TensorFrame {
                    step: i,
                    rows: 2,
                    cols: 2,
                    src_rows: None,
                    src_cols: None,
                    data: vec![i as f32; 4],
                },
            );
        }
        let ring = d.tensors.get("W").unwrap();
        assert_eq!(ring.len(), MAX_TENSOR_FRAMES);
        assert_eq!(ring.back().unwrap().step, 49);
        assert_eq!(ring.front().unwrap().step, 18); // 50 - 32
    }

    #[test]
    fn timing_eviction_preserves_per_name_history() {
        // Matches the TS quirk: eviction fires only when crossing MAX_TIMINGS,
        // and at that moment each name is trimmed to its last 500 samples. The
        // point is that a busy series can't evict another's history to zero.
        let mut d = TrainingData::new();
        for i in 0..3000 {
            d.push_timing("fast", 1.0, i); // no eviction yet (< 5000)
        }
        assert_eq!(d.current.timings.len(), 3000);
        for i in 0..3000 {
            d.push_timing("slow", 2.0, i); // crosses 5000 at the 2001st slow
        }
        let fast = d.current.timings.iter().filter(|t| t.name == "fast").count();
        let slow = d.current.timings.iter().filter(|t| t.name == "slow").count();
        // "fast" was trimmed to exactly its last 500 (not wiped), proving the
        // per-name floor; "slow" keeps 500 kept at eviction + the 999 pushed
        // afterward (no further eviction until 5000 is crossed again).
        assert_eq!(fast, MAX_TIMINGS_PER_NAME);
        assert_eq!(slow, MAX_TIMINGS_PER_NAME + 999);
    }

    #[test]
    fn clear_snapshots_ghost() {
        let mut d = TrainingData::new();
        d.push_scalar("loss", 0, 1.0);
        d.push_scalar("loss", 1, 0.5);
        d.clear();
        assert!(d.current.scalars.is_empty());
        assert_eq!(d.previous.scalars.get("loss").unwrap().len(), 2);
    }

    #[test]
    fn clear_without_data_keeps_ghost_empty() {
        let mut d = TrainingData::new();
        d.clear();
        assert!(d.previous.scalars.is_empty());
    }
}
