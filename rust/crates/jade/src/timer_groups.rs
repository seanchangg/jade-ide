//! Timer bundles: several probe timers aggregated into one synthetic series
//! (e.g. `embedForward` + `linearForward` + … → "Forward").
//!
//! The probe's timing `step` is a per-command-buffer counter, so every sample
//! has a unique step and "sum the samples at step N" is meaningless. Instead
//! the aggregator detects loop **cycles**: inside a training loop each member
//! fires once per iteration, so a member arriving *twice* means the loop
//! wrapped — flush the pending window as one summed point. A full window
//! (every member seen once) flushes immediately, so well-ordered loops emit
//! exactly one point per iteration without waiting for the wrap.
//!
//! Group *definitions* persist per-workspace (`workspace.json`, the kernel
//! names are project-specific); the group's checked/unchecked state rides the
//! same prefs as every other timer under its synthetic name.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A persisted bundle definition: the synthetic timer name + member timers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerGroupDef {
    pub name: String,
    pub members: Vec<String>,
}

/// One group's in-flight cycle window.
#[derive(Debug, Default)]
struct Window {
    pending: HashMap<String, f64>,
}

/// Aggregates member timing samples into per-cycle group points.
#[derive(Debug, Default)]
pub struct GroupAggregator {
    defs: Vec<TimerGroupDef>,
    windows: Vec<Window>,
}

impl GroupAggregator {
    pub fn new(defs: Vec<TimerGroupDef>) -> Self {
        let windows = defs.iter().map(|_| Window::default()).collect();
        GroupAggregator { defs, windows }
    }

    pub fn defs(&self) -> &[TimerGroupDef] {
        &self.defs
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Groups that contain `member` (a timer may be bundled more than once).
    pub fn groups_of(&self, member: &str) -> Vec<&str> {
        self.defs
            .iter()
            .filter(|d| d.members.iter().any(|m| m == member))
            .map(|d| d.name.as_str())
            .collect()
    }

    pub fn contains_member(&self, member: &str) -> bool {
        self.defs
            .iter()
            .any(|d| d.members.iter().any(|m| m == member))
    }

    pub fn get(&self, name: &str) -> Option<&TimerGroupDef> {
        self.defs.iter().find(|d| d.name == name)
    }

    /// Add (or replace) a bundle. Empty member lists are ignored.
    pub fn add(&mut self, name: &str, mut members: Vec<String>) {
        members.dedup();
        if members.is_empty() {
            return;
        }
        self.remove(name);
        self.defs.push(TimerGroupDef {
            name: name.to_string(),
            members,
        });
        self.windows.push(Window::default());
    }

    /// Dissolve a bundle (member timers are untouched).
    pub fn remove(&mut self, name: &str) {
        if let Some(i) = self.defs.iter().position(|d| d.name == name) {
            self.defs.remove(i);
            self.windows.remove(i);
        }
    }

    /// Feed one member sample. Returns the group points this sample completed,
    /// as `(group_name, summed_ms, step)` — at most one per containing group.
    pub fn on_sample(&mut self, name: &str, ms: f64, step: i64) -> Vec<(String, f64, i64)> {
        let mut out = Vec::new();
        for (def, win) in self.defs.iter().zip(self.windows.iter_mut()) {
            if !def.members.iter().any(|m| m == name) {
                continue;
            }
            // Cycle wrap: this member already reported → the loop started a
            // new iteration; flush the previous window first.
            if win.pending.contains_key(name) {
                out.push((def.name.clone(), win.pending.values().sum(), step));
                win.pending.clear();
            }
            win.pending.insert(name.to_string(), ms);
            // Full window: every member reported → the iteration is complete.
            if win.pending.len() == def.members.len() {
                out.push((def.name.clone(), win.pending.values().sum(), step));
                win.pending.clear();
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fwd() -> GroupAggregator {
        GroupAggregator::new(vec![TimerGroupDef {
            name: "Forward".to_string(),
            members: vec!["embed".into(), "linear".into(), "loss".into()],
        }])
    }

    #[test]
    fn full_window_emits_sum() {
        let mut g = fwd();
        assert!(g.on_sample("embed", 1.0, 10).is_empty());
        assert!(g.on_sample("linear", 2.0, 11).is_empty());
        let out = g.on_sample("loss", 3.0, 12);
        assert_eq!(out, vec![("Forward".to_string(), 6.0, 12)]);
        // Window cleared: the next cycle starts fresh.
        assert!(g.on_sample("embed", 1.5, 13).is_empty());
    }

    #[test]
    fn cycle_wrap_flushes_partial_window() {
        let mut g = fwd();
        // "loss" never fires this iteration (e.g. skipped branch); the loop
        // wraps when "embed" reports again — flush what we have.
        assert!(g.on_sample("embed", 1.0, 0).is_empty());
        assert!(g.on_sample("linear", 2.0, 1).is_empty());
        let out = g.on_sample("embed", 1.5, 2);
        assert_eq!(out, vec![("Forward".to_string(), 3.0, 2)]);
        // The wrapping sample starts the NEW window.
        assert!(g.on_sample("linear", 2.5, 3).is_empty());
        let out = g.on_sample("loss", 3.0, 4);
        assert_eq!(out, vec![("Forward".to_string(), 7.0, 4)]);
    }

    #[test]
    fn non_members_pass_through() {
        let mut g = fwd();
        assert!(g.on_sample("backward", 9.0, 0).is_empty());
        assert!(!g.contains_member("backward"));
        assert!(g.contains_member("embed"));
    }

    #[test]
    fn member_in_two_groups_feeds_both() {
        let mut g = fwd();
        g.add("All", vec!["embed".into(), "backward".into()]);
        assert!(g.on_sample("backward", 4.0, 0).is_empty());
        let out = g.on_sample("embed", 1.0, 1);
        // Completes "All" (both members) but not "Forward".
        assert_eq!(out, vec![("All".to_string(), 5.0, 1)]);
        assert_eq!(g.groups_of("embed"), vec!["Forward", "All"]);
    }

    #[test]
    fn add_replaces_and_remove_dissolves() {
        let mut g = fwd();
        g.add("Forward", vec!["a".into(), "b".into()]);
        assert_eq!(g.defs().len(), 1);
        assert_eq!(g.get("Forward").unwrap().members, vec!["a", "b"]);
        g.remove("Forward");
        assert!(g.is_empty());
        g.remove("Forward"); // idempotent
    }
}
