//! Sequential migrations driven by `PRAGMA user_version`.
//!
//! Each entry is applied inside its own transaction, and the version is bumped in
//! the same transaction so a failed migration leaves the file untouched.

use rusqlite::Connection;

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
    (
        8,
        include_str!("../../migrations/0008_encode_queue.sql"),
        "encode queue: the documents that still owe the vector index",
    ),
    (
        9,
        include_str!("../../migrations/0009_drop_coherent.sql"),
        "drop coherent: the split-review verdict leaves the schema",
    ),
    (
        10,
        include_str!("../../migrations/0010_rename_doc_hash.sql"),
        "rename doc_hash -> encode_hash: the column says it is this row's embedding state",
    ),
    (
        11,
        include_str!("../../migrations/0011_memory_versions.sql"),
        "memory versions: the state a document change replaces",
    ),
];

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
