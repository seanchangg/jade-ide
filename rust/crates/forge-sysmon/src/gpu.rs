//! Best-effort macOS GPU utilization via `ioreg`. Port of the
//! `systeminformation`-backed GPU sampling in `system-monitor.ts` (TS :62-77);
//! on macOS that library shells out to `ioreg` and reads the same
//! `IOAccelerator` `PerformanceStatistics` dictionary we parse here.

use tokio::process::Command;

/// Run `ioreg` and parse GPU utilization + model. Returns `None` (leaving the
/// caller's cached value untouched, TS :75-77) if `ioreg` can't be run or its
/// output lacks the utilization key.
pub(crate) async fn sample_gpu() -> Option<(i64, String)> {
    // `ioreg -r -d 1 -w0 -c IOAccelerator` — the systeminformation macOS query.
    let output = Command::new("ioreg")
        .args(["-r", "-d", "1", "-w0", "-c", "IOAccelerator"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_ioreg_utilization(&text)
}

/// Extract `("Device Utilization %"=N, model)` from `ioreg` output.
///
/// The relevant lines look like (captured on an Apple M4, 2026-07-13):
/// ```text
///   "PerformanceStatistics" = {…,"Device Utilization %"=3,…}
///   "model" = "Apple M4"
/// ```
/// Returns `None` when the utilization key is absent (the signal that GPU data
/// is unavailable — the caller then reports `-1` / `"unavailable"`).
pub fn parse_ioreg_utilization(text: &str) -> Option<(i64, String)> {
    let percent = find_int_after(text, "\"Device Utilization %\"=")?;
    let name = find_model(text).unwrap_or_else(|| "unknown".to_string());
    Some((percent, name))
}

/// Read the integer immediately following `key` (e.g. `"Device Utilization %"=`).
fn find_int_after(text: &str, key: &str) -> Option<i64> {
    let idx = text.find(key)? + key.len();
    let rest = &text[idx..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Read the GPU model from a `"model" = "Apple M4"` line.
fn find_model(text: &str) -> Option<String> {
    let key = "\"model\" = \"";
    let idx = text.find(key)? + key.len();
    let rest = &text[idx..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `ioreg -r -d 1 -w0 -c IOAccelerator` excerpt captured on this
    /// machine (Apple M4) on 2026-07-13 — the two lines the parser needs out of
    /// the full ~44KB dump.
    const FIXTURE: &str = r#"
    +-o IOAccelerator  <class IOAccelerator>
      "PerformanceStatistics" = {"In use system memory (driver)"=0,"Alloc system memory"=7630618624,"Tiler Utilization %"=3,"recoveryCount"=21,"lastRecoveryTime"=32881655804519,"Renderer Utilization %"=3,"TiledSceneBytes"=950272,"Device Utilization %"=3,"SplitSceneCount"=0,"Allocated PB Size"=77201408,"In use system memory"=550649856}
      "model" = "Apple M4"
"#;

    #[test]
    fn parses_device_utilization_and_model() {
        let (pct, name) = parse_ioreg_utilization(FIXTURE).expect("fixture has the key");
        assert_eq!(pct, 3);
        assert_eq!(name, "Apple M4");
    }

    #[test]
    fn picks_device_not_tiler_or_renderer_utilization() {
        // "Device Utilization %" must not be confused with the adjacent
        // "Tiler"/"Renderer" utilization keys.
        let text = r#""Tiler Utilization %"=11,"Renderer Utilization %"=22,"Device Utilization %"=42"#;
        assert_eq!(parse_ioreg_utilization(text).unwrap().0, 42);
    }

    #[test]
    fn missing_key_yields_none() {
        assert!(parse_ioreg_utilization("no gpu stats here").is_none());
    }

    #[test]
    fn model_defaults_to_unknown_when_absent() {
        let text = r#""Device Utilization %"=7"#;
        assert_eq!(parse_ioreg_utilization(text).unwrap(), (7, "unknown".to_string()));
    }
}
