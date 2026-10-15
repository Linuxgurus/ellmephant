//! Sequential migrations driven by `PRAGMA user_version`.
//!
//! Each entry is applied inside its own transaction, and the version is bumped in
//! the same transaction so a failed migration leaves the file untouched.

use rusqlite::Connection;
use tracing::{debug, info};

use crate::error::Result;

/// `(version, sql, label)` in ascending order. SQL is embedded at compile time so
/// the binary carries its own schema.
const MIGRATIONS: &[(i32, &str, &str)] = &[
    (
        1,
        include_str!("../../migrations/0001_init.sql"),
        "init: memory, links, meta",
    ),
    (
        2,
        include_str!("../../migrations/0002_telemetry.sql"),
        "telemetry: visits, was_useful",
    ),
    (
        3,
        include_str!("../../migrations/0003_times_useful.sql"),
        "rename was_useful -> times_useful",
    ),
    (
        4,
        include_str!("../../migrations/0004_coherent.sql"),
        "coherent: split-review verdict",
    ),
    (
        5,
        include_str!("../../migrations/0005_projects.sql"),
        "projects: project, project_memory",
    ),
    (
        6,
        include_str!("../../migrations/0006_link_type.sql"),
        "link type: bare keys, memory_link.to_type",
    ),
    (
        7,
        include_str!("../../migrations/0007_drop_project_memory.sql"),
        "retire the one-home association: drop project_memory",
    ),
];

/// Current schema version recorded in the file.
pub fn version(conn: &Connection) -> Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Apply every migration newer than the file's `user_version`.
///
/// Loud when something HAPPENS and quiet otherwise, on purpose: this runs on every store open, so a
/// no-op must not spam `info` — but a schema change is the single most important thing that can
/// happen to a store at startup, and it used to happen in total silence.
pub fn run(conn: &Connection) -> Result<()> {
    let mut current = version(conn)?;
    let latest = latest();
    if current >= latest {
        debug!(schema = current, "schema up to date; no migration to apply");
        return Ok(());
    }
    info!(from = current, to = latest, "migrating the store");
    let mut applied = 0;
    for (version, sql, label) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        let started = std::time::Instant::now();
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        current = *version;
        applied += 1;
        info!(
            schema = version,
            label = %label,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "migration applied"
        );
    }
    info!(schema = current, applied, "migrations complete");
    Ok(())
}

/// Highest version this binary knows how to create.
pub fn latest() -> i32 {
    MIGRATIONS.iter().map(|(v, _, _)| *v).max().unwrap_or(0)
}
