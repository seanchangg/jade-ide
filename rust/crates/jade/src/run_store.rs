//! Persistent run store — the "experiment tracking" layer of the research
//! studio. Each telemetry-producing run (Run or Debug) is written to
//! `<workspace>/.jade/runs.db` (SQLite) at process exit: its scalar series,
//! timer samples, memory curve, and metadata (git sha, exe, duration, exit
//! code). The training view lists stored runs and overlays any of them on the
//! Loss/Memory charts — generalizing the single in-memory ghost run.
//!
//! Design notes:
//! - Whole-run writes in one transaction at run end (series are capped by
//!   `training.rs` at 1000 points / 5000 timings, so a run is a few thousand
//!   rows at most — no streaming needed).
//! - Runs with no telemetry at all are not stored: a plain `./a.out` without
//!   the probe shouldn't pile noise into the run list.
//! - All store operations are best-effort from the app's point of view;
//!   callers treat a dead store (`open` failure) as "feature off".

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

use crate::training::{MemPoint, RunData, ScalarPoint, Stats, TimingPoint};

/// Cap on rows returned by [`RunStore::list_runs`]; the UI shows fewer.
pub const MAX_LISTED_RUNS: usize = 100;

/// How a stored run was launched (the two telemetry-producing paths).
pub const KIND_RUN: &str = "run";
pub const KIND_DEBUG: &str = "debug";

/// Metadata for one stored run (the `runs` table row, sans series).
#[derive(Debug, Clone, PartialEq)]
pub struct RunMeta {
    pub id: i64,
    /// Executable file name (the run list's display label).
    pub label: String,
    /// [`KIND_RUN`] or [`KIND_DEBUG`].
    pub kind: String,
    /// Unix seconds at run start.
    pub started_epoch: i64,
    pub duration_ms: Option<i64>,
    pub exit_code: Option<i64>,
    /// Short git sha of the workspace at run start, `+dirty` suffix if the
    /// tree had uncommitted changes. `None` outside a git repo.
    pub git_sha: Option<String>,
    /// Number of distinct scalar series stored (run-list hint).
    pub scalar_series: i64,
}

/// Captured at run start; combined with the end-of-run data in [`RunStore::save_run`].
#[derive(Debug, Clone)]
pub struct PendingRun {
    pub label: String,
    pub kind: &'static str,
    pub started_epoch: i64,
    pub git_sha: Option<String>,
}

impl PendingRun {
    /// Snapshot the launch context now (start time + git state).
    pub fn begin(label: String, kind: &'static str, workspace_root: &Path) -> Self {
        PendingRun {
            label,
            kind,
            started_epoch: now_epoch(),
            git_sha: git_sha(workspace_root),
        }
    }
}

pub struct RunStore {
    conn: Connection,
}

impl RunStore {
    /// Open (creating if needed) the workspace's run DB at `.jade/runs.db`.
    pub fn open(workspace_root: &Path) -> rusqlite::Result<Self> {
        let dir = workspace_root.join(".jade");
        let _ = std::fs::create_dir_all(&dir);
        Self::open_at(&dir.join("runs.db"))
    }

    /// Open at an explicit path (test seam; `:memory:` friendly via `open_in_memory`).
    pub fn open_at(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// In-memory store for tests.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS runs (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 label         TEXT NOT NULL,
                 kind          TEXT NOT NULL,
                 started_epoch INTEGER NOT NULL,
                 duration_ms   INTEGER,
                 exit_code     INTEGER,
                 git_sha       TEXT
             );
             CREATE TABLE IF NOT EXISTS scalars (
                 run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                 name   TEXT NOT NULL,
                 step   INTEGER NOT NULL,
                 value  REAL NOT NULL
             );
             CREATE INDEX IF NOT EXISTS scalars_by_run ON scalars(run_id, name);
             CREATE TABLE IF NOT EXISTS timings (
                 run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                 name   TEXT NOT NULL,
                 step   INTEGER NOT NULL,
                 ms     REAL NOT NULL
             );
             CREATE INDEX IF NOT EXISTS timings_by_run ON timings(run_id);
             CREATE TABLE IF NOT EXISTS memory (
                 run_id    INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
                 idx       INTEGER NOT NULL,
                 heap_used REAL NOT NULL
             );
             CREATE INDEX IF NOT EXISTS memory_by_run ON memory(run_id);",
        )?;
        Ok(RunStore { conn })
    }

    /// Persist one finished run in a single transaction. Returns the new run
    /// id, or `None` when `data` holds no telemetry (nothing worth storing).
    pub fn save_run(
        &mut self,
        pending: &PendingRun,
        data: &RunData,
        duration_ms: Option<i64>,
        exit_code: Option<i64>,
    ) -> rusqlite::Result<Option<i64>> {
        let empty = data.scalars.values().all(|v| v.is_empty())
            && data.timings.is_empty()
            && data.memory.is_empty();
        if empty {
            return Ok(None);
        }

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO runs (label, kind, started_epoch, duration_ms, exit_code, git_sha)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                pending.label,
                pending.kind,
                pending.started_epoch,
                duration_ms,
                exit_code,
                pending.git_sha,
            ],
        )?;
        let run_id = tx.last_insert_rowid();

        {
            let mut ins = tx.prepare(
                "INSERT INTO scalars (run_id, name, step, value) VALUES (?1, ?2, ?3, ?4)",
            )?;
            // Deterministic series order keeps saves reproducible in tests.
            let mut names: Vec<&String> = data.scalars.keys().collect();
            names.sort();
            for name in names {
                for p in &data.scalars[name] {
                    ins.execute(params![run_id, name, p.step, p.value])?;
                }
            }
        }
        {
            let mut ins = tx
                .prepare("INSERT INTO timings (run_id, name, step, ms) VALUES (?1, ?2, ?3, ?4)")?;
            for t in &data.timings {
                ins.execute(params![run_id, t.name, t.step, t.ms])?;
            }
        }
        {
            let mut ins =
                tx.prepare("INSERT INTO memory (run_id, idx, heap_used) VALUES (?1, ?2, ?3)")?;
            for (i, m) in data.memory.iter().enumerate() {
                ins.execute(params![run_id, i as i64, m.heap_used])?;
            }
        }

        tx.commit()?;
        Ok(Some(run_id))
    }

    /// Stored runs, newest first, capped at [`MAX_LISTED_RUNS`].
    pub fn list_runs(&self) -> rusqlite::Result<Vec<RunMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.label, r.kind, r.started_epoch, r.duration_ms, r.exit_code, r.git_sha,
                    (SELECT COUNT(DISTINCT name) FROM scalars s WHERE s.run_id = r.id)
             FROM runs r ORDER BY r.id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![MAX_LISTED_RUNS as i64], |row| {
            Ok(RunMeta {
                id: row.get(0)?,
                label: row.get(1)?,
                kind: row.get(2)?,
                started_epoch: row.get(3)?,
                duration_ms: row.get(4)?,
                exit_code: row.get(5)?,
                git_sha: row.get(6)?,
                scalar_series: row.get(7)?,
            })
        })?;
        rows.collect()
    }

    /// Load one run's series back as a [`RunData`] (stats recomputed), for
    /// chart overlays. `None` for unknown ids.
    pub fn load_run(&self, id: i64) -> rusqlite::Result<Option<RunData>> {
        let exists: bool = self
            .conn
            .query_row("SELECT 1 FROM runs WHERE id = ?1", params![id], |_| Ok(true))
            .unwrap_or(false);
        if !exists {
            return Ok(None);
        }

        let mut data = RunData::default();

        let mut stmt = self.conn.prepare(
            "SELECT name, step, value FROM scalars WHERE run_id = ?1 ORDER BY name, rowid",
        )?;
        let mut rows = stmt.query(params![id])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let p = ScalarPoint { step: row.get(1)?, value: row.get(2)? };
            let st = data.scalar_stats.entry(name.clone()).or_insert(Stats {
                min: f64::INFINITY,
                max: f64::NEG_INFINITY,
            });
            st.min = st.min.min(p.value);
            st.max = st.max.max(p.value);
            data.scalars.entry(name).or_default().push(p);
        }

        let mut stmt = self
            .conn
            .prepare("SELECT name, step, ms FROM timings WHERE run_id = ?1 ORDER BY rowid")?;
        let mut rows = stmt.query(params![id])?;
        while let Some(row) = rows.next()? {
            let ms: f64 = row.get(2)?;
            data.timing_max = data.timing_max.max(ms);
            data.timings.push(TimingPoint {
                name: row.get(0)?,
                step: row.get(1)?,
                ms,
            });
        }

        let mut stmt = self
            .conn
            .prepare("SELECT heap_used FROM memory WHERE run_id = ?1 ORDER BY idx")?;
        let mut rows = stmt.query(params![id])?;
        while let Some(row) = rows.next()? {
            let heap_used: f64 = row.get(0)?;
            data.memory.push(MemPoint { heap_used });
            data.memory_max = data.memory_max.max(heap_used);
        }

        Ok(Some(data))
    }

    /// Delete a stored run (series rows cascade).
    pub fn delete_run(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM runs WHERE id = ?1", params![id])?;
        Ok(())
    }
}

/// Unix seconds now.
pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Compact age string for the run list ("now", "5m", "2h", "3d").
pub fn age_string(started_epoch: i64, now_epoch: i64) -> String {
    let s = (now_epoch - started_epoch).max(0);
    match s {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Short git sha of `root`'s HEAD with a `+dirty` suffix when the tree has
/// uncommitted changes; `None` outside a repo / without git. Two quick
/// subprocesses at run start (~5ms) — not on any hot path.
fn git_sha(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        return None;
    }
    let dirty = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if dirty {
        sha.push_str("+dirty");
    }
    Some(sha)
}

/// Test seam for [`git_sha`]'s path handling without a real repo.
#[allow(dead_code)]
pub fn db_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".jade").join("runs.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_data() -> RunData {
        let mut d = crate::training::TrainingData::new();
        d.push_scalar("loss", 0, 1.0);
        d.push_scalar("loss", 1, 0.5);
        d.push_scalar("lr", 0, 0.001);
        d.push_timing("matmul", 3.2, 0);
        d.push_timing("matmul", 3.4, 1);
        d.push_memory(1024.0);
        d.push_memory(2048.0);
        d.current
    }

    fn pending(kind: &'static str) -> PendingRun {
        PendingRun {
            label: "a.out".to_string(),
            kind,
            started_epoch: 1_700_000_000,
            git_sha: Some("abc123def456".to_string()),
        }
    }

    #[test]
    fn save_load_roundtrip() {
        let mut store = RunStore::open_in_memory().unwrap();
        let id = store
            .save_run(&pending(KIND_RUN), &sample_data(), Some(1234), Some(0))
            .unwrap()
            .expect("non-empty run stored");

        let runs = store.list_runs().unwrap();
        assert_eq!(runs.len(), 1);
        let meta = &runs[0];
        assert_eq!(meta.id, id);
        assert_eq!(meta.label, "a.out");
        assert_eq!(meta.kind, KIND_RUN);
        assert_eq!(meta.duration_ms, Some(1234));
        assert_eq!(meta.exit_code, Some(0));
        assert_eq!(meta.git_sha.as_deref(), Some("abc123def456"));
        assert_eq!(meta.scalar_series, 2); // loss + lr

        let data = store.load_run(id).unwrap().expect("run exists");
        let loss = data.scalars.get("loss").unwrap();
        assert_eq!(loss.len(), 2);
        assert_eq!((loss[0].step, loss[0].value), (0, 1.0));
        assert_eq!((loss[1].step, loss[1].value), (1, 0.5));
        // Stats recomputed on load.
        let st = data.scalar_stats.get("loss").unwrap();
        assert_eq!((st.min, st.max), (0.5, 1.0));
        assert_eq!(data.timings.len(), 2);
        assert_eq!(data.timings[0].name, "matmul");
        assert_eq!(data.memory.len(), 2);
        assert_eq!(data.memory_max, 2048.0);
    }

    #[test]
    fn empty_run_not_stored() {
        let mut store = RunStore::open_in_memory().unwrap();
        let id = store
            .save_run(&pending(KIND_RUN), &RunData::default(), Some(10), Some(0))
            .unwrap();
        assert_eq!(id, None);
        assert!(store.list_runs().unwrap().is_empty());
    }

    #[test]
    fn list_newest_first_and_delete_cascades() {
        let mut store = RunStore::open_in_memory().unwrap();
        let a = store
            .save_run(&pending(KIND_RUN), &sample_data(), Some(1), Some(0))
            .unwrap()
            .unwrap();
        let b = store
            .save_run(&pending(KIND_DEBUG), &sample_data(), Some(2), None)
            .unwrap()
            .unwrap();

        let runs = store.list_runs().unwrap();
        assert_eq!(runs.iter().map(|r| r.id).collect::<Vec<_>>(), vec![b, a]);
        assert_eq!(runs[0].kind, KIND_DEBUG);

        store.delete_run(a).unwrap();
        assert_eq!(store.list_runs().unwrap().len(), 1);
        assert!(store.load_run(a).unwrap().is_none());
        // Cascade removed the series rows too.
        let n: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM scalars WHERE run_id = ?1", params![a], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn load_unknown_run_is_none() {
        let store = RunStore::open_in_memory().unwrap();
        assert!(store.load_run(42).unwrap().is_none());
    }

    #[test]
    fn ages_format_compactly() {
        let t = 1_700_000_000;
        assert_eq!(age_string(t, t + 5), "now");
        assert_eq!(age_string(t, t + 300), "5m");
        assert_eq!(age_string(t, t + 7200), "2h");
        assert_eq!(age_string(t, t + 3 * 86_400), "3d");
        // Clock skew never yields negatives.
        assert_eq!(age_string(t, t - 100), "now");
    }

    #[test]
    fn on_disk_roundtrip_creates_jade_dir() {
        let dir = std::env::temp_dir().join(format!("jade-runstore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut store = RunStore::open(&dir).unwrap();
        assert!(dir.join(".jade").join("runs.db").exists());
        store
            .save_run(&pending(KIND_RUN), &sample_data(), None, Some(1))
            .unwrap()
            .unwrap();
        drop(store);

        // Reopen and read back.
        let store = RunStore::open(&dir).unwrap();
        assert_eq!(store.list_runs().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
