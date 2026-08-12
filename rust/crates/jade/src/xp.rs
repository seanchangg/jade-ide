//! XP bar (§4.10) — the gamification port of `editor/xp-bar.ts`.
//!
//! Pure, deterministic, unit-tested: the per-edit credit rule, the streak state
//! machine (with an **injectable clock** so the 10s window is testable without
//! real timers), and the level curve. The app owns the UI mount (tab-bar right)
//! and the global `xpTotal` persistence; [`XpStore`] provides the load/save +
//! Electron `jade-config.json` migration.

use std::path::{Path, PathBuf};

/// A qualifying edit that keeps arriving within this window keeps the streak
/// alive; a longer gap resets it to 1 (TS `STREAK_TIMEOUT_MS`, :7).
pub const STREAK_TIMEOUT_MS: u64 = 10_000;
/// Base XP per credited line (TS `XP_PER_LINE_BASE`, :8).
pub const XP_PER_LINE_BASE: u64 = 1;
/// XP for L1→L2 (TS `LEVEL_BASE`, :11).
pub const LEVEL_BASE: u64 = 150;
/// Extra XP each subsequent level costs (TS `LEVEL_STEP`, :12).
pub const LEVEL_STEP: u64 = 100;

/// The decomposition of a total XP count into level + progress within it
/// (TS `levelFromXp`, :14-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelInfo {
    pub level: u64,
    pub progress: u64,
    pub needed: u64,
}

/// Level N→N+1 costs `150 + (N-1)*100`; walk levels until the remainder fits.
pub fn level_from_xp(total_xp: u64) -> LevelInfo {
    let mut level = 1u64;
    let mut needed = LEVEL_BASE;
    let mut remaining = total_xp;
    while remaining >= needed {
        remaining -= needed;
        level += 1;
        needed += LEVEL_STEP;
    }
    LevelInfo {
        level,
        progress: remaining,
        needed,
    }
}

/// Strip a trailing `//` line comment (TS `.replace(/\/\/.*$/, '')`), so
/// `foo(); // done` still ends with `;`.
fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Does an inserted change earn a credit (TS `attachXpToEditor`, :71-85)? Credit
/// only when the change inserts a newline, is ≤300 chars (anti-paste), and the
/// completed line — comments stripped — ends with `;`.
pub fn edit_earns_credit(inserted: &str, completed_line: &str) -> bool {
    if !inserted.contains('\n') {
        return false;
    }
    if inserted.chars().count() > 300 {
        return false;
    }
    strip_line_comment(completed_line).trim().ends_with(';')
}

/// The running XP + streak state (TS module-level `totalXp`/`streak`/`streakTimer`).
#[derive(Debug, Clone)]
pub struct XpState {
    total: u64,
    streak: u64,
    /// Monotonic ms of the last credited edit (`None` until the first).
    last_credit_ms: Option<u64>,
}

impl XpState {
    pub fn new(total: u64) -> Self {
        XpState {
            total,
            streak: 1,
            last_credit_ms: None,
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// The streak that *would* apply for a credit at `now_ms`: 1 once the 10s
    /// window since the last credit has lapsed, else the live streak. This is
    /// what the `×N` badge shows (the TS timer resets it after 10s idle).
    pub fn effective_streak(&self, now_ms: u64) -> u64 {
        if self.window_active(now_ms) {
            self.streak
        } else {
            1
        }
    }

    fn window_active(&self, now_ms: u64) -> bool {
        self.last_credit_ms
            .map(|t| now_ms.saturating_sub(t) <= STREAK_TIMEOUT_MS)
            .unwrap_or(false)
    }

    /// Apply `credits` qualifying lines at `now_ms`; returns the XP gained.
    ///
    /// Order matches the TS exactly (:88-99): XP is added using the *current*
    /// streak, then — only if a prior credit is still inside the window — the
    /// streak increments for next time. A lapsed window resets the streak to 1
    /// before crediting.
    pub fn credit(&mut self, credits: u64, now_ms: u64) -> u64 {
        if credits == 0 {
            return 0;
        }
        let active = self.window_active(now_ms);
        if !active {
            self.streak = 1;
        }
        let gained = XP_PER_LINE_BASE * self.streak * credits;
        self.total += gained;
        if active {
            self.streak += 1;
        }
        self.last_credit_ms = Some(now_ms);
        gained
    }
}

/// Global `xpTotal` persistence (§1.1). Our file lives in `~/.config/jade/`
/// (alongside `telemetry.json`); on first run it migrates the Electron app's
/// `jade-config.json` `xpTotal` if present.
pub struct XpStore {
    path: Option<PathBuf>,
}

impl XpStore {
    /// Our global config path: `~/.config/jade/xp.json`.
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".config/jade/xp.json"))
    }

    /// The Electron app's global config for migration:
    /// `~/Library/Application Support/Jade/jade-config.json`.
    pub fn electron_config_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library/Application Support/Jade/jade-config.json"),
        )
    }

    pub fn load() -> (Self, u64) {
        let path = Self::default_path();
        let electron = Self::electron_config_path();
        let total = load_total(path.as_deref(), electron.as_deref());
        (XpStore { path }, total)
    }

    pub fn save(&self, total: u64) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let json = serde_json::json!({ "xpTotal": total });
        if let Ok(s) = serde_json::to_string_pretty(&json) {
            let _ = std::fs::write(path, s);
        }
    }
}

/// Read `xpTotal` from our file, else migrate the Electron `jade-config.json`,
/// else 0. Both are `{ "xpTotal": <number> }`-shaped; errors → 0.
pub fn load_total(our_path: Option<&Path>, electron_path: Option<&Path>) -> u64 {
    if let Some(p) = our_path {
        if let Some(v) = read_xp_total(p) {
            return v;
        }
    }
    if let Some(p) = electron_path {
        if let Some(v) = read_xp_total(p) {
            return v;
        }
    }
    0
}

fn read_xp_total(path: &Path) -> Option<u64> {
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("xpTotal").and_then(|x| x.as_u64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_curve_matches_ts() {
        // L1 needs 150, L2 needs 250, L3 needs 350…
        assert_eq!(level_from_xp(0), LevelInfo { level: 1, progress: 0, needed: 150 });
        assert_eq!(level_from_xp(149), LevelInfo { level: 1, progress: 149, needed: 150 });
        assert_eq!(level_from_xp(150), LevelInfo { level: 2, progress: 0, needed: 250 });
        assert_eq!(level_from_xp(150 + 249), LevelInfo { level: 2, progress: 249, needed: 250 });
        assert_eq!(level_from_xp(150 + 250), LevelInfo { level: 3, progress: 0, needed: 350 });
    }

    #[test]
    fn credit_rule_requires_newline_semicolon_and_length() {
        assert!(edit_earns_credit("\n", "int x = 1;"));
        assert!(edit_earns_credit("int x = 1;\n", "int x = 1;"));
        // Comment stripped, still ends with ;.
        assert!(edit_earns_credit("\n", "foo(); // done"));
        // No newline in the change → no credit (plain typing).
        assert!(!edit_earns_credit("x", "int x = 1;"));
        // Completed line doesn't end with ; (unfinished / plain Enter).
        assert!(!edit_earns_credit("\n", "if (x) {"));
        // Anti-paste: >300 chars.
        let huge = format!("{}\n", "a".repeat(400));
        assert!(!edit_earns_credit(&huge, "a;"));
    }

    #[test]
    fn streak_increments_within_window_and_scales_xp() {
        let mut s = XpState::new(0);
        // First credit at t=0: streak stays 1, +1.
        assert_eq!(s.credit(1, 0), 1);
        assert_eq!(s.total(), 1);
        // Second at t=5s (within 10s): +1 at streak 1, then streak→2.
        assert_eq!(s.credit(1, 5_000), 1);
        assert_eq!(s.total(), 2);
        // Third at t=8s: +2 at streak 2, then streak→3.
        assert_eq!(s.credit(1, 8_000), 2);
        assert_eq!(s.total(), 4);
        assert_eq!(s.effective_streak(8_000), 3);
    }

    #[test]
    fn streak_resets_after_window_lapses() {
        let mut s = XpState::new(0);
        s.credit(1, 0);
        s.credit(1, 5_000); // streak now 2
        // Big gap (>10s): resets to 1 before crediting.
        assert_eq!(s.credit(1, 25_000), 1);
        assert_eq!(s.total(), 3);
        // Idle badge shows 1 once the window lapses.
        assert_eq!(s.effective_streak(40_000), 1);
    }

    #[test]
    fn multiple_credits_in_one_edit() {
        let mut s = XpState::new(10);
        // Two qualifying lines in one change, first credit → streak 1.
        assert_eq!(s.credit(2, 0), 2);
        assert_eq!(s.total(), 12);
    }

    #[test]
    fn store_roundtrip_and_migration() {
        let base = std::env::temp_dir().join(format!("jade-xp-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let ours = base.join("xp.json");
        let electron = base.join("jade-config.json");

        // No files → 0.
        assert_eq!(load_total(Some(&ours), Some(&electron)), 0);

        // Electron config present, ours absent → migrate.
        std::fs::write(&electron, r#"{"xpTotal": 777, "theme": "dark"}"#).unwrap();
        assert_eq!(load_total(Some(&ours), Some(&electron)), 777);

        // Our file wins once written.
        std::fs::write(&ours, r#"{"xpTotal": 42}"#).unwrap();
        assert_eq!(load_total(Some(&ours), Some(&electron)), 42);

        let _ = std::fs::remove_dir_all(&base);
    }
}
