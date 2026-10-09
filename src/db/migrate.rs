//! Sequential migrations driven by `PRAGMA user_version`.
//!
//! Each entry is applied inside its own transaction, and the version is bumped in
//! the same transaction so a failed migration leaves the file untouched.

use rusqlite::Connection;

use crate::error::Result;

/// `(version, sql, label)` in ascending order. SQL is embedded at compile time so
/// the binary carries its own schema.
const MIGRATIONS: &[(i32, &str, &str)] = &[(
    1,
    include_str!("../../migrations/0001_init.sql"),
    "init: memory, links, meta",
)];

/// Current schema version recorded in the file.
pub fn version(conn: &Connection) -> Result<i32> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Apply every migration newer than the file's `user_version`.
pub fn run(conn: &Connection) -> Result<()> {
    let mut current = version(conn)?;
    for (version, sql, _label) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        current = *version;
    }
    Ok(())
}

/// Highest version this binary knows how to create.
pub fn latest() -> i32 {
    MIGRATIONS.iter().map(|(v, _, _)| *v).max().unwrap_or(0)
}
