//! Training data buffers (feature inventory §7.1, §10). Pure state feeding the
//! Loss / Memory / Kernel-time / Timing / Tensor visualizations.
//!
//! Caps, matching the TS `TrainingView`:
//!   • scalars & memory: last **1000** points
//!   • tensors: ring of **32** frames per buffer
//!   • timings: **5000** total, eviction preserving recent history per name
//!
//! `clear()` resets everything — each run starts with clean charts. Cross-run
//! comparison is the run store's job (explicit overlays from the RUNS list),
//! which replaced the old automatic ghost-previous-run underlay.
//!
//! Stats (`scalar_stats`, `memory_max`, `timing_max`) are **monotone over the
//! whole run**, never recomputed from the retained window: recomputing after
//! eviction dropped the early extremes made every chart rescale mid-run (the
//! "scale jumps once compaction starts" artifact).

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

/// One run's worth of series data (the live run, or a stored run loaded for
/// an overlay). All `*_stats`/`*_max` fields are run-global monotone extremes,
/// NOT window extremes — stable scales across eviction.
#[derive(Debug, Default, Clone)]
pub struct RunData {
    pub scalars: HashMap<String, Vec<ScalarPoint>>,
    pub scalar_stats: HashMap<String, Stats>,
    pub timings: Vec<TimingPoint>,
    /// Largest single timing sample seen this run (kernel chart's shared scale).
    pub timing_max: f64,
    /// Largest sample per series name — each pipeline's independent chart
    /// scale. Monotone like `timing_max`, so eviction never rescales a curve.
    pub timing_max_by_name: HashMap<String, f64>,
    pub memory: Vec<MemPoint>,
    pub memory_max: f64,
}

#[derive(Debug, Default)]
pub struct TrainingData {
    pub current: RunData,
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
        }
        // Monotone run-global extremes: NEVER recomputed from the retained
        // window, so eviction can't move the chart scale.
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

    pub fn push_timing(&mut self, name: &str, ms: f64, step: i64) {
        self.current.timings.push(TimingPoint {
            name: name.to_string(),
            ms,
            step,
        });
        if ms > self.current.timing_max {
            self.current.timing_max = ms; // monotone shared kernel scale
        }
        let per = self
            .current
            .timing_max_by_name
            .entry(name.to_string())
            .or_insert(0.0);
        if ms > *per {
            *per = ms; // monotone per-series scale
        }
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
        }
        if heap_used > self.current.memory_max {
            self.current.memory_max = heap_used; // monotone run peak
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

    /// Reset everything — each run starts with clean charts. Compare runs via
    /// the run store's explicit overlays, not an automatic ghost.
    pub fn clear(&mut self) {
        self.current = RunData::default();
        self.tensors.clear();
    }
}

/// Trim the buffer while keeping the most recent samples of each timer name
/// (so one busy timer can't evict another's history) — training-view.ts:233-244.
///
/// Deviation from the TS: eviction targets a LOW-water mark (80% of
/// `MAX_TIMINGS`) and shrinks the per-name cap when there are many distinct
/// names. The TS's fixed 500-per-name floor meant that with > 10 timer names
/// the buffer could sit above `MAX_TIMINGS` forever, so *every* subsequent
/// push re-ran this O(n) scan — a main-thread stall at real telemetry rates
/// (measured: ~90% of the UI thread in here during a metalLLM run). Trimming
/// to 80% makes eviction amortized O(1) per push: it runs once per ~1000
/// pushes, not once per push.
fn evict_timings(timings: &mut Vec<TimingPoint>) {
    const LOW_WATER: usize = MAX_TIMINGS * 4 / 5;

    // Per-name cap: the classic 500, shrunk when many distinct names would
    // otherwise exceed the low-water total.
    let names: std::collections::HashSet<&str> =
        timings.iter().map(|t| t.name.as_str()).collect();
    let per_name = (LOW_WATER / names.len().max(1))
        .clamp(1, MAX_TIMINGS_PER_NAME);

    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut keep = vec![true; timings.len()];
    for i in (0..timings.len()).rev() {
        let name = timings[i].name.as_str();
        let c = counts.entry(name).or_insert(0);
        *c += 1;
        if *c > per_name {
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
        // Stats stay run-global (monotone): eviction must NOT move the scale —
        // recomputing from the window made charts rescale on every compaction.
        let st = d.current.scalar_stats.get("loss").unwrap();
        assert_eq!(st.min, 0.0);
        assert_eq!(st.max, (MAX_POINTS + 249) as f64);
    }

    #[test]
    fn memory_and_timing_scales_survive_eviction() {
        let mut d = TrainingData::new();
        // A huge early peak, then small steady values past the cap.
        d.push_memory(1_000_000.0);
        for _ in 0..(MAX_POINTS + 50) {
            d.push_memory(10.0);
        }
        assert_eq!(d.current.memory.len(), MAX_POINTS);
        assert_eq!(d.current.memory_max, 1_000_000.0, "peak is run-global");

        // Same for the kernel scale: a first-iteration spike outlives eviction.
        d.push_timing("k", 88.0, 0);
        for i in 0..(MAX_TIMINGS + 10) {
            d.push_timing("k", 1.0, i as i64 + 1);
        }
        assert_eq!(d.current.timing_max, 88.0);
        assert_eq!(d.current.timing_max_by_name["k"], 88.0);
    }

    #[test]
    fn per_series_maxes_are_independent() {
        let mut d = TrainingData::new();
        d.push_timing("attn", 5.0, 0);
        d.push_timing("embed", 0.05, 1);
        d.push_timing("attn", 4.0, 2);
        d.push_timing("embed", 0.04, 3);
        // Each pipeline scales to ITS OWN peak, not the biggest timer's.
        assert_eq!(d.current.timing_max_by_name["attn"], 5.0);
        assert_eq!(d.current.timing_max_by_name["embed"], 0.05);
        assert_eq!(d.current.timing_max, 5.0);
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
    fn timing_eviction_bounds_buffer_with_many_names() {
        // Regression: with > MAX_TIMINGS/MAX_TIMINGS_PER_NAME distinct timer
        // names (a real metalLLM run has per-layer kernels), the old fixed
        // 500-per-name floor left the buffer above MAX_TIMINGS forever, so
        // every push re-ran the O(n) eviction — a main-thread stall. Eviction
        // must land at/below the 80% low-water mark so it stays amortized.
        let mut timings: Vec<TimingPoint> = (0..(MAX_TIMINGS + 1))
            .map(|i| TimingPoint {
                name: format!("kernel{}", i % 20), // 20 distinct names
                ms: 1.0,
                step: i as i64,
            })
            .collect();
        evict_timings(&mut timings);
        assert!(
            timings.len() <= MAX_TIMINGS * 4 / 5,
            "one eviction must reach the low-water mark, got {}",
            timings.len()
        );
        // Every name keeps a fair share of recent history.
        for n in 0..20 {
            let name = format!("kernel{n}");
            let c = timings.iter().filter(|t| t.name == name).count();
            assert!(c > 0, "{name} evicted to zero");
        }
        // And the buffer stays bounded through continued pushes.
        let mut d = TrainingData::new();
        for i in 0..20_000 {
            d.push_timing(&format!("kernel{}", i % 20), 1.0, i);
        }
        assert!(d.current.timings.len() <= MAX_TIMINGS + 1);
    }

    #[test]
    fn clear_resets_everything() {
        let mut d = TrainingData::new();
        d.push_scalar("loss", 0, 1.0);
        d.push_timing("k", 2.0, 0);
        d.push_memory(64.0);
        d.clear();
        assert!(d.current.scalars.is_empty());
        assert!(d.current.timings.is_empty());
        assert!(d.current.memory.is_empty());
        assert_eq!(d.current.memory_max, 0.0);
        assert_eq!(d.current.timing_max, 0.0);
        assert!(d.current.timing_max_by_name.is_empty());
        assert!(d.current.scalar_stats.is_empty());
    }
}
