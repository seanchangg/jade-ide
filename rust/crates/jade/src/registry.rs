//! Renderer-side telemetry registry — the discovered-item model behind the
//! SCALARS / TIMERS / BUFFERS sidebar (feature inventory §5.6, ported from
//! `panels/telemetry-panel.ts`).
//!
//! This is pure state: no sockets, no filesystem. The app layer performs the
//! side effects (persisting prefs, sending `track` to the probe) in response to
//! the outcomes returned here. Keeping it pure makes the auto-check rule and
//! ordering unit-testable.

use std::collections::HashMap;

use jade_telemetry::Kind;

use crate::prefs::TelemetryPrefs;

/// First N discovered scalars are auto-checked so the loss chart isn't empty by
/// default, unless the user has a stored preference (§10, telemetry-panel.ts:40).
pub const AUTO_CHECK_SCALARS: usize = 3;

/// Default per-dimension streaming cap for buffers when the user hasn't set one
/// (telemetry-panel.ts:39 `DEFAULT_MAX_DIM = 256` — the full detail the probe
/// streams; the ≤64/≤128/≤256 chips lower it per buffer).
pub const DEFAULT_MAX_DIM: u32 = 256;

#[derive(Debug, Clone)]
pub struct RegistryItem {
    pub kind: Kind,
    pub name: String,
    pub enabled: bool,
    /// Metadata dims from the buffer's decl/frame (true source shape).
    pub meta_rows: Option<u32>,
    pub meta_cols: Option<u32>,
    /// User-provided shape hint (persisted), overrides meta for streaming.
    pub shape_rows: Option<u32>,
    pub shape_cols: Option<u32>,
    /// Per-buffer streaming resolution override.
    pub max_dim: Option<u32>,
    /// Live readouts.
    pub last_value: Option<f64>, // scalar: value; timer: ms
    pub last_step: Option<i64>,
    pub last_rows: Option<u32>,
    pub last_cols: Option<u32>,
    /// Pre-populated from a persisted pref (or a loaded bundle def) before the
    /// probe confirmed it this session. Seeded items render in the panels but
    /// don't count as discovered inventory; the first real decl clears this
    /// and re-arms `track`.
    pub seeded: bool,
}

impl RegistryItem {
    fn new(kind: Kind, name: &str) -> Self {
        RegistryItem {
            kind,
            name: name.to_string(),
            enabled: false,
            meta_rows: None,
            meta_cols: None,
            shape_rows: None,
            shape_cols: None,
            max_dim: None,
            last_value: None,
            last_step: None,
            last_rows: None,
            last_cols: None,
            seeded: false,
        }
    }

    /// The shape to advertise to the probe: user hint first, else meta.
    pub fn effective_shape(&self) -> Option<(u32, u32)> {
        match (self.shape_rows, self.shape_cols) {
            (Some(r), Some(c)) => Some((r, c)),
            _ => None,
        }
    }
}

/// What `declare`/`note_*` changed, so the caller can react (send `track`,
/// repaint). `auto_enabled` means a scalar was auto-checked and its `track`
/// must now be pushed to the probe.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeclareOutcome {
    pub structure_changed: bool,
    pub auto_enabled: bool,
    /// A stored preference enabled this item at first declaration — its
    /// `track` (with persisted maxDim/shape) must be pushed to the server,
    /// whose registry starts empty each session. The Electron renderer only
    /// applied stored prefs at rename time (telemetry-panel.ts:154-158), so
    /// pref-enabled buffers that never renamed silently stayed dark there.
    pub pref_enabled: bool,
}

pub fn key_of(kind: Kind, name: &str) -> String {
    format!("{} {}", kind.as_str(), name)
}

/// Byte-wise compare with digit runs compared as numbers, so `buf#9` sorts
/// before `buf#10`. Dotted-scope grouping falls out of plain prefix ordering.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    loop {
        match (a.get(i), b.get(j)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(&ca), Some(&cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let prse = |s: &[u8], k: &mut usize| -> u64 {
                        let mut v: u64 = 0;
                        while let Some(c) = s.get(*k).filter(|c| c.is_ascii_digit()) {
                            v = v.saturating_mul(10).saturating_add((c - b'0') as u64);
                            *k += 1;
                        }
                        v
                    };
                    match prse(a, &mut i).cmp(&prse(b, &mut j)) {
                        Ordering::Equal => {}
                        o => return o,
                    }
                } else {
                    match ca.cmp(&cb) {
                        Ordering::Equal => {
                            i += 1;
                            j += 1;
                        }
                        o => return o,
                    }
                }
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct TelemetryRegistry {
    items: HashMap<String, RegistryItem>,
    /// Insertion order for stable UI rendering.
    order: Vec<String>,
    /// Discovery order of scalars, for the auto-check-first-3 rule.
    scalar_discovery: Vec<String>,
}

impl TelemetryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_enabled(&self, kind: Kind, name: &str) -> bool {
        self.items
            .get(&key_of(kind, name))
            .map(|i| i.enabled)
            .unwrap_or(false)
    }

    pub fn get(&self, kind: Kind, name: &str) -> Option<&RegistryItem> {
        self.items.get(&key_of(kind, name))
    }

    /// Items of one kind, in discovery order (for the sidebar sections).
    pub fn items_of_kind(&self, kind: Kind) -> Vec<&RegistryItem> {
        self.order
            .iter()
            .filter_map(|k| self.items.get(k))
            .filter(|i| i.kind == kind)
            .collect()
    }

    /// Items of one kind sorted by name (natural number ordering), so dotted
    /// scope siblings — every `blocks.attn.*` buffer — sit together instead of
    /// interleaving in probe allocation order, and `#9` sorts before `#10`.
    /// The BUFFERS section uses this; scalars/timers keep discovery order.
    pub fn items_of_kind_grouped(&self, kind: Kind) -> Vec<&RegistryItem> {
        let mut items = self.items_of_kind(kind);
        items.sort_by(|a, b| natural_cmp(&a.name, &b.name));
        items
    }

    /// Register (or merge metadata into) an item. Applies stored prefs and the
    /// auto-check rule on first sight.
    pub fn declare(
        &mut self,
        kind: Kind,
        name: &str,
        meta_rows: Option<u32>,
        meta_cols: Option<u32>,
        prefs: &TelemetryPrefs,
    ) -> DeclareOutcome {
        let key = key_of(kind, name);
        if let Some(item) = self.items.get_mut(&key) {
            // Merge late-arriving metadata (e.g. buffer shape from first frame).
            let mut changed = false;
            if meta_rows.is_some() && item.meta_rows != meta_rows {
                item.meta_rows = meta_rows;
                changed = true;
            }
            if meta_cols.is_some() && item.meta_cols != meta_cols {
                item.meta_cols = meta_cols;
                changed = true;
            }
            // First REAL decl of a pref-seeded item: confirm it and surface
            // `pref_enabled` once so the caller re-arms `track` — the probe's
            // registry starts empty each run and a seeded checkbox would
            // otherwise never stream.
            if item.seeded {
                item.seeded = false;
                return DeclareOutcome {
                    structure_changed: true,
                    auto_enabled: false,
                    pref_enabled: item.enabled,
                };
            }
            return DeclareOutcome {
                structure_changed: changed,
                auto_enabled: false,
                pref_enabled: false,
            };
        }

        let mut item = RegistryItem::new(kind, name);
        item.meta_rows = meta_rows;
        item.meta_cols = meta_cols;

        // Restore persisted shape hint / resolution override.
        if let Some((r, c)) = prefs.shape_pref(&key) {
            item.shape_rows = Some(r);
            item.shape_cols = Some(c);
        }
        if let Some(d) = prefs.maxdim_pref(&key) {
            item.max_dim = Some(d);
        }

        // Enabled state: stored pref wins; else auto-check first 3 scalars.
        let mut auto_enabled = false;
        let mut pref_enabled = false;
        match prefs.enabled_pref(&key) {
            Some(pref) => {
                item.enabled = pref;
                pref_enabled = pref;
            }
            None if kind == Kind::Scalar => {
                self.scalar_discovery.push(name.to_string());
                if self.scalar_discovery.len() <= AUTO_CHECK_SCALARS {
                    item.enabled = true;
                    auto_enabled = true;
                }
            }
            None => {}
        }

        self.items.insert(key.clone(), item);
        self.order.push(key);
        DeclareOutcome {
            structure_changed: true,
            auto_enabled,
            pref_enabled,
        }
    }

    /// Pre-populate an item before the probe has declared it this session
    /// (persisted selections / loaded bundle defs), so the sidebar and pre-run
    /// panel aren't empty at startup. Stored prefs (enabled/shape/maxdim)
    /// apply as usual; the item is marked `seeded` so it doesn't count as
    /// discovered inventory and re-arms `track` on its first real decl.
    /// No-op if the item already exists.
    pub fn seed(&mut self, kind: Kind, name: &str, prefs: &TelemetryPrefs) {
        let key = key_of(kind, name);
        if self.items.contains_key(&key) {
            return;
        }
        self.declare(kind, name, None, None, prefs);
        if let Some(item) = self.items.get_mut(&key) {
            item.seeded = true;
        }
    }

    /// Remove every still-seeded item except those `keep` claims, returning
    /// what was removed. Called after a scan/run has produced real decls:
    /// anything the probe didn't confirm by then is a stale leftover name.
    pub fn prune_seeded(&mut self, keep: impl Fn(Kind, &str) -> bool) -> Vec<(Kind, String)> {
        let doomed: Vec<String> = self
            .items
            .iter()
            .filter(|(_, i)| i.seeded && !keep(i.kind, &i.name))
            .map(|(k, _)| k.clone())
            .collect();
        let mut removed = Vec::new();
        for key in doomed {
            if let Some(item) = self.items.remove(&key) {
                self.order.retain(|k| k != &key);
                removed.push((item.kind, item.name));
            }
        }
        removed
    }

    /// Toggle enabled state (user checkbox). Returns whether it changed.
    pub fn set_enabled(&mut self, kind: Kind, name: &str, enabled: bool) -> bool {
        match self.items.get_mut(&key_of(kind, name)) {
            Some(item) if item.enabled != enabled => {
                item.enabled = enabled;
                true
            }
            _ => false,
        }
    }

    /// Apply a user shape hint (buffers). Returns whether it changed.
    pub fn set_shape(&mut self, kind: Kind, name: &str, rows: u32, cols: u32) -> bool {
        if rows == 0 || cols == 0 {
            return false;
        }
        let item = self
            .items
            .entry(key_of(kind, name))
            .or_insert_with(|| {
                self.order.push(key_of(kind, name));
                RegistryItem::new(kind, name)
            });
        if item.shape_rows == Some(rows) && item.shape_cols == Some(cols) {
            return false;
        }
        item.shape_rows = Some(rows);
        item.shape_cols = Some(cols);
        true
    }

    pub fn set_max_dim(&mut self, kind: Kind, name: &str, max_dim: u32) -> bool {
        let clamped = max_dim.clamp(16, 256);
        if let Some(item) = self.items.get_mut(&key_of(kind, name)) {
            if item.max_dim != Some(clamped) {
                item.max_dim = Some(clamped);
                return true;
            }
        }
        false
    }

    pub fn note_scalar(
        &mut self,
        name: &str,
        step: i64,
        value: f64,
        prefs: &TelemetryPrefs,
    ) -> DeclareOutcome {
        let outcome = self.declare(Kind::Scalar, name, None, None, prefs);
        if let Some(item) = self.items.get_mut(&key_of(Kind::Scalar, name)) {
            item.last_value = Some(value);
            item.last_step = Some(step);
        }
        outcome
    }

    pub fn note_timing(
        &mut self,
        name: &str,
        ms: f64,
        step: i64,
        prefs: &TelemetryPrefs,
    ) -> DeclareOutcome {
        let outcome = self.declare(Kind::Timer, name, None, None, prefs);
        if let Some(item) = self.items.get_mut(&key_of(Kind::Timer, name)) {
            item.last_value = Some(ms);
            item.last_step = Some(step);
        }
        outcome
    }

    pub fn note_tensor(
        &mut self,
        name: &str,
        rows: u32,
        cols: u32,
        step: i64,
        prefs: &TelemetryPrefs,
    ) -> DeclareOutcome {
        // Register under SOURCE dims so the sidebar shows the true tensor shape.
        let outcome = self.declare(Kind::Buffer, name, Some(rows), Some(cols), prefs);
        if let Some(item) = self.items.get_mut(&key_of(Kind::Buffer, name)) {
            item.last_rows = Some(rows);
            item.last_cols = Some(cols);
            item.last_step = Some(step);
        }
        outcome
    }

    /// Migrate an item to a new name (buffer symbolication), preserving enabled
    /// state and applying `to`'s stored preference. Pure registry move; the
    /// caller migrates the pref file.
    pub fn rename(&mut self, kind: Kind, from: &str, to: &str, prefs: &TelemetryPrefs) -> bool {
        let from_key = key_of(kind, from);
        let to_key = key_of(kind, to);
        if from_key == to_key {
            return false;
        }
        let Some(mut item) = self.items.remove(&from_key) else {
            return false;
        };
        self.order.retain(|k| k != &from_key);
        if self.items.contains_key(&to_key) {
            return true; // destination exists; drop the placeholder
        }
        item.name = to.to_string();
        // Apply the destination's persisted preference, if any (e.g. a buffer
        // the user enabled last session, only now known by its real name).
        if let Some(pref) = prefs.enabled_pref(&to_key) {
            item.enabled = pref;
        }
        if let Some((r, c)) = prefs.shape_pref(&to_key) {
            item.shape_rows = Some(r);
            item.shape_cols = Some(c);
        }
        self.items.insert(to_key.clone(), item);
        self.order.push(to_key);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs() -> TelemetryPrefs {
        TelemetryPrefs::default()
    }

    #[test]
    fn grouped_buffers_sort_by_scope_with_natural_numbers() {
        let mut r = TelemetryRegistry::new();
        let p = prefs();
        // Probe allocation order: layer 1's objects, then layer 2's renames
        // land as "#2" siblings, plus a straggler that never resolved.
        for n in [
            "blocks.ln1.xBuffer",
            "blocks.attn.XBuffer",
            "blocks.attn.QBuffer",
            "blocks.mlp.vBuffer",
            "blocks.ln1.xBuffer#2",
            "blocks.attn.XBuffer#10",
            "blocks.attn.XBuffer#2",
            "MetalMLP::MetalMLP #118",
        ] {
            r.declare(Kind::Buffer, n, None, None, &p);
        }
        let names: Vec<&str> = r
            .items_of_kind_grouped(Kind::Buffer)
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "MetalMLP::MetalMLP #118",
                "blocks.attn.QBuffer",
                "blocks.attn.XBuffer",
                "blocks.attn.XBuffer#2",  // natural: #2 before #10
                "blocks.attn.XBuffer#10",
                "blocks.ln1.xBuffer",
                "blocks.ln1.xBuffer#2",
                "blocks.mlp.vBuffer",
            ]
        );
        // Discovery order stays untouched for the other accessor.
        assert_eq!(r.items_of_kind(Kind::Buffer)[0].name, "blocks.ln1.xBuffer");
    }

    #[test]
    fn auto_checks_first_three_scalars_only() {
        let mut r = TelemetryRegistry::new();
        let p = prefs();
        let names = ["loss", "acc", "lr", "grad_norm", "temp"];
        let mut auto = vec![];
        for n in names {
            let out = r.declare(Kind::Scalar, n, None, None, &p);
            auto.push(out.auto_enabled);
        }
        assert_eq!(auto, vec![true, true, true, false, false]);
        assert!(r.is_enabled(Kind::Scalar, "loss"));
        assert!(!r.is_enabled(Kind::Scalar, "grad_norm"));
    }

    #[test]
    fn stored_pref_overrides_autocheck() {
        let mut r = TelemetryRegistry::new();
        let mut p = prefs();
        p.set_enabled("scalar loss", false); // user turned it off before
        let out = r.declare(Kind::Scalar, "loss", None, None, &p);
        assert!(!out.auto_enabled);
        assert!(!r.is_enabled(Kind::Scalar, "loss"));
    }

    #[test]
    fn buffers_never_autocheck() {
        let mut r = TelemetryRegistry::new();
        let p = prefs();
        let out = r.declare(Kind::Buffer, "weights", Some(4), Some(4), &p);
        assert!(!out.auto_enabled);
        assert!(!r.is_enabled(Kind::Buffer, "weights"));
    }

    #[test]
    fn declare_is_idempotent_and_merges_meta() {
        let mut r = TelemetryRegistry::new();
        let p = prefs();
        let o1 = r.declare(Kind::Buffer, "W", None, None, &p);
        assert!(o1.structure_changed);
        let o2 = r.declare(Kind::Buffer, "W", Some(8), Some(16), &p);
        assert!(o2.structure_changed); // meta arrived
        let o3 = r.declare(Kind::Buffer, "W", Some(8), Some(16), &p);
        assert!(!o3.structure_changed); // no change
        let item = r.get(Kind::Buffer, "W").unwrap();
        assert_eq!((item.meta_rows, item.meta_cols), (Some(8), Some(16)));
    }

    #[test]
    fn set_enabled_reports_change() {
        let mut r = TelemetryRegistry::new();
        r.declare(Kind::Timer, "matmul", None, None, &prefs());
        assert!(r.set_enabled(Kind::Timer, "matmul", true));
        assert!(!r.set_enabled(Kind::Timer, "matmul", true)); // no-op
        assert!(r.is_enabled(Kind::Timer, "matmul"));
    }

    #[test]
    fn rename_preserves_enabled_and_order() {
        let mut r = TelemetryRegistry::new();
        let p = prefs();
        r.declare(Kind::Buffer, "Matrix #3", None, None, &p);
        r.set_enabled(Kind::Buffer, "Matrix #3", true);
        assert!(r.rename(Kind::Buffer, "Matrix #3", "weights", &p));
        assert!(r.is_enabled(Kind::Buffer, "weights"));
        assert!(r.get(Kind::Buffer, "Matrix #3").is_none());
        assert_eq!(r.items_of_kind(Kind::Buffer).len(), 1);
    }
}
