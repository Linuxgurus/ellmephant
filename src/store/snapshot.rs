//! A CONSISTENT COPY OF A LIVE STORE — the quiesce the backups never had.
//!
//! A file-level copy of a live SQLite database is not atomic: `memory.db` can be captured
//! mid-transaction, and in WAL the truth is split across `memory.db`, `-wal` and `-shm`. Velero's
//! fs-backup copies the volume as FILES, so "we have backups" was true of the files and optimistic
//! about the data ([[store-backups-velero-kopia]], `prj:agent-restore`).
//!
//! `VACUUM INTO` is SQLite's own answer: it writes a NEW database holding the committed state at one
//! point in time, and a reader never blocks a writer in WAL. So the store produces its own artifact
//! and the backup copies a file that is consistent BY CONSTRUCTION.
//!
//! ★ WHAT IT COSTS: one extra copy of the database on the volume, and the write while it runs.
//! What it BUYS: a restore no longer has to hope the copy landed between two transactions.
//!
//! ★ WHY A TIMER HERE AND NOT A VELERO PRE-HOOK: the container has no `sqlite3` binary, and a pod
//! annotation that exists to run one RESTARTS the pod — which on `edge` + `Always` is a DEPLOY, not a
//! backup step ([[restart-is-a-deploy]]). A timer inside the process that already owns the database
//! needs neither.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, params};

use crate::error::Result;

/// `<root>/snapshot/` — the directory Velero copies along with everything else.
pub fn dir(root: &Path) -> PathBuf {
    root.join("snapshot")
}

/// `<root>/snapshot/memory.db` — the consistent artifact.
pub fn file(root: &Path) -> PathBuf {
    dir(root).join("memory.db")
}

/// Write a snapshot and return its path. ATOMIC: build a unique temp file, then rename over the
/// old one, so a reader (or a backup that starts mid-write) never sees a half-written database.
pub fn write(conn: &Connection, root: &Path) -> Result<PathBuf> {
    let d = dir(root);
    std::fs::create_dir_all(&d)?;
    // `VACUUM INTO` REFUSES an existing target, so the temp name is unique per process AND removed
    // first: a killed run must not poison the next one.
    let tmp = d.join(format!(".memory.db.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    conn.execute("VACUUM INTO ?1", params![tmp.to_string_lossy()])?;
    let out = file(root);
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

/// Age of the newest snapshot in seconds; `None` when there is none. This is what a health probe (and
/// so a bell) can read to answer "is the store being quiesced?" — see [`crate::store::snapshot`].
pub fn age_seconds(root: &Path) -> Option<i64> {
    let mtime = std::fs::metadata(file(root)).ok()?.modified().ok()?;
    mtime.elapsed().ok().map(|d| d.as_secs() as i64)
}

/// How often the server writes a snapshot. Zero or a bad value disables it (a number the operator
/// cannot parse should not silently become a different number).
pub fn every() -> Option<Duration> {
    let hours: u64 = std::env::var("ELLMEPHANT_SNAPSHOT_HOURS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(6);
    (hours > 0).then(|| Duration::from_secs(hours * 3600))
}
