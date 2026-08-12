//! Telemetry preference persistence — the Rust analogue of the renderer's
//! `localStorage` `jade.telemetry.*` keys (feature inventory §1.3, §5.6).
//!
//! ## Storage
//! One JSON file at `~/.config/jade/telemetry.json`. Read errors are swallowed
//! to an empty set (same forgiving behavior as the Electron config reader).
//!
//! ## Key mapping (localStorage → this file)
//! The renderer stored three flat key spaces, each suffixed by the item's
//! `"<kind> <name>"` identifier (`kind` ∈ scalar|timer|buffer):
//!
//! | localStorage key                          | JSON location                         |
//! |-------------------------------------------|---------------------------------------|
//! | `jade.telemetry.enabled.<kind> <name>`   | `enabled["<kind> <name>"] : bool`     |
//! | `jade.telemetry.shape.<kind> <name>`     | `shape["<kind> <name>"]   : "RxC"`    |
//! | `jade.telemetry.maxdim.<kind> <name>`    | `maxdim["<kind> <name>"]  : u32`      |
//!
//! `shape` values keep the exact `"RxC"` string form the TS used, so a future
//! migration script can copy localStorage values in verbatim. The `enabled`
//! value was `"1"`/`"0"` in localStorage and is a JSON bool here.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TelemetryPrefs {
    /// key = `"<kind> <name>"`
    #[serde(default)]
    pub enabled: HashMap<String, bool>,
    /// key = `"<kind> <name>"`, value = `"RxC"`
    #[serde(default)]
    pub shape: HashMap<String, String>,
    /// key = `"<kind> <name>"`
    #[serde(default)]
    pub maxdim: HashMap<String, u32>,

    #[serde(skip)]
    path: Option<PathBuf>,
}

impl TelemetryPrefs {
    /// Default path: `~/.config/jade/telemetry.json`.
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".config/jade/telemetry.json"))
    }

    /// Load from the default path, swallowing all errors to an empty set.
    pub fn load() -> TelemetryPrefs {
        match Self::default_path() {
            Some(path) => {
                let mut prefs = Self::load_from(&path);
                prefs.path = Some(path);
                prefs
            }
            None => TelemetryPrefs::default(),
        }
    }

    /// Load from an explicit path (used by tests). Missing/corrupt → empty.
    pub fn load_from(path: &PathBuf) -> TelemetryPrefs {
        let mut prefs = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<TelemetryPrefs>(&s).ok())
            .unwrap_or_default();
        prefs.path = Some(path.clone());
        prefs
    }

    /// Persist to the configured path (creating parent dirs). Errors swallowed.
    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    pub fn enabled_pref(&self, key: &str) -> Option<bool> {
        self.enabled.get(key).copied()
    }

    pub fn set_enabled(&mut self, key: &str, value: bool) {
        self.enabled.insert(key.to_string(), value);
    }

    pub fn shape_pref(&self, key: &str) -> Option<(u32, u32)> {
        self.shape.get(key).and_then(|s| parse_shape(s))
    }

    pub fn set_shape(&mut self, key: &str, rows: u32, cols: u32) {
        self.shape.insert(key.to_string(), format!("{}x{}", rows, cols));
    }

    pub fn maxdim_pref(&self, key: &str) -> Option<u32> {
        self.maxdim.get(key).copied()
    }

    pub fn set_maxdim(&mut self, key: &str, max_dim: u32) {
        self.maxdim.insert(key.to_string(), max_dim);
    }

    /// Migrate all three prefs from a placeholder key to its real name (buffer
    /// symbolication rename). Destination values win if already present.
    pub fn migrate(&mut self, from_key: &str, to_key: &str) {
        if let Some(v) = self.enabled.remove(from_key) {
            self.enabled.entry(to_key.to_string()).or_insert(v);
        }
        if let Some(v) = self.shape.remove(from_key) {
            self.shape.entry(to_key.to_string()).or_insert(v);
        }
        if let Some(v) = self.maxdim.remove(from_key) {
            self.maxdim.entry(to_key.to_string()).or_insert(v);
        }
    }
}

/// Parse the `"RxC"` shape string the TS renderer stored.
pub fn parse_shape(s: &str) -> Option<(u32, u32)> {
    let (r, c) = s.split_once('x')?;
    Some((r.trim().parse().ok()?, c.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_roundtrip() {
        let mut p = TelemetryPrefs::default();
        p.set_shape("buffer W", 256, 384);
        assert_eq!(p.shape.get("buffer W").unwrap(), "256x384");
        assert_eq!(p.shape_pref("buffer W"), Some((256, 384)));
    }

    #[test]
    fn parse_shape_variants() {
        assert_eq!(parse_shape("4x8"), Some((4, 8)));
        assert_eq!(parse_shape("bad"), None);
    }

    #[test]
    fn migrate_preserves_dest() {
        let mut p = TelemetryPrefs::default();
        p.set_enabled("buffer #3", true);
        p.set_shape("buffer #3", 2, 2);
        p.migrate("buffer #3", "buffer weights");
        assert_eq!(p.enabled_pref("buffer weights"), Some(true));
        assert_eq!(p.shape_pref("buffer weights"), Some((2, 2)));
        assert_eq!(p.enabled_pref("buffer #3"), None);
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("jade-prefs-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("telemetry.json");
        let mut p = TelemetryPrefs::load_from(&path);
        p.set_enabled("scalar loss", true);
        p.set_maxdim("buffer W", 128);
        p.save();

        let loaded = TelemetryPrefs::load_from(&path);
        assert_eq!(loaded.enabled_pref("scalar loss"), Some(true));
        assert_eq!(loaded.maxdim_pref("buffer W"), Some(128));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
