//! Database open + schema migration. One writer connection; readers open their own.

pub mod meta;
pub mod migrate;

use std::time::Duration;

use rusqlite::Connection;

use crate::config::Paths;
use crate::error::Result;

/// Open (creating if needed) the store, apply pragmas, and run migrations.
pub fn open(paths: &Paths) -> Result<Connection> {
    paths.ensure()?;
    // Auto-extensions only attach to connections created *after* registration, so this
    // must run before `Connection::open` (PLAN §5 S0.1).
    crate::index::vec::register_extension();
    let conn = Connection::open(paths.db())?;
    configure(&conn)?;
    migrate::run(&conn)?;
    // ★ EAGER INDEX, NOT LAZY (2026-10-21): the vec0 tables used to appear on the first *embed*, which
    // meant a rolled store served an EMPTY semantic index until somebody happened to write — and said
    // nothing about why. Worse for the shape change that moved `memory_vec` into the partitioned
    // `document_vec`: the migration lived in `ensure`, so a store with a quiet queue never migrated and
    // simply searched nothing. Creating (and migrating) the derived index at open makes "the store is
    // up" and "the index exists" the same event. A reader still opens read-only and touches no DDL.
    if let Some(dim) = crate::index::vec::stored_dim(&conn)? {
        crate::index::vec::ensure(&conn, dim)?;
        // ★ AND A CHANGED PROJECTION PURGES HERE, before this connection can serve a write. A refill
        // cannot fix a mixed index (a row with a vector is skipped even though its vector is from
        // another model), so the only safe answer is: no vectors at all, every document queued, one
        // transaction. See `maintain::reindex_if_stale` for the sequence that motivated it.
        crate::store::maintain::reindex_if_stale(&conn)?;
    }
    Ok(conn)
}

/// Open the store READ-ONLY: a connection of the caller's own, for a read that must not queue behind
/// a writer. No migration, and no pragma that writes — in WAL mode a reader sees the last committed
/// state alongside a writer, which is what `/health` has always relied on from outside the session.
pub fn open_read_only(paths: &Paths) -> Result<Connection> {
    crate::index::vec::register_extension();
    let conn = Connection::open_with_flags(paths.db(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_millis(5_000))?;
    Ok(conn)
}

/// PRAGMAs from SPEC §7 / PLAN §2 #10. `journal_mode` returns a row, so it is
/// queried rather than executed.
pub fn configure(conn: &Connection) -> Result<()> {
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_millis(5_000))?;
    Ok(())
}
