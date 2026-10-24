//! Database open + schema migration. One writer connection; readers open their own.

pub mod meta;
pub mod migrate;

use std::time::Duration;

use rusqlite::Connection;

use crate::config::Paths;
use crate::error::Result;

/// Open (creating if needed) the store, apply pragmas, and run migrations.
///
/// The rebuild budget is read from `config.toml` here because the purge happens at OPEN, before any
/// caller sees the connection (soma/ellmephant#76). A caller that already holds a [`Config`] should
/// use [`open_with`] so the decision is made with the settings the process actually runs with.
pub fn open(paths: &Paths) -> Result<Connection> {
    let cfg = crate::config::Config::load(paths)?;
    open_with(paths, &cfg)
}

/// [`open`] with an explicit config — the path a server takes, so the budget is not re-read from disk
/// and cannot disagree with the running settings.
pub fn open_with(paths: &Paths, cfg: &crate::config::Config) -> Result<Connection> {
    paths.ensure()?;
    // Auto-extensions only attach to connections created *after* registration, so this
    // must run before `Connection::open` (PLAN §5 S0.1).
    crate::index::vec::register_extension();
    let conn = Connection::open(paths.db())?;
    configure(&conn)?;
    migrate::run(&conn)?;
    // ★ SUDO POLICY IS READ AT BOOT AND NEVER FATAL (#109): authority is granted by configuration, so
    // a malformed line is a warning with a name, not a store that refuses to open.
    crate::store::sudo::load_at_boot(&conn);
    // ★ THE TREE IS CONFIGURATION, loaded the same way (2026-10-24): a fresh store's pens are NAMES
    // until somebody attaches them, and no door can.
    crate::store::load_tree_at_boot(&conn);
    // Case-folded uniqueness: the index when the table is clean, a named warning when it is not.
    crate::store::ensure_name_uniqueness(&conn)?;
    // ★ RETENTION THAT ACTUALLY RUNS (found reviewing #129). `audit::prune` existed and nothing called
    // it, which makes "retention is enforced" a sentence rather than a fact. Pruning at open costs one
    // DELETE on boot and needs no bell; a table that grows forever is a second store nobody maintains.
    let _ = crate::store::audit::prune(&conn, crate::store::audit::KEEP_DAYS);
    // ★ EAGER INDEX, NOT LAZY (2026-10-21): the vec0 tables used to appear on the first *embed*, which
    // meant a rolled store served an EMPTY semantic index until somebody happened to write — and said
    // nothing about why. Worse for the shape change that moved `memory_vec` into the partitioned
    // `document_vec`: the migration lived in `ensure`, so a store with a quiet queue never migrated and
    // simply searched nothing. Creating (and migrating) the derived index at open makes "the store is
    // up" and "the index exists" the same event. A reader still opens read-only and touches no DDL.
    if let Some(dim) = crate::index::vec::stored_dim(&conn)? {
        crate::index::vec::ensure(&conn, dim)?;
        // ★ AND A CHANGED PROJECTION PURGES HERE, before this connection can serve a write — if the
        // rebuild fits its budget. A refusal leaves the index untouched and STALE (see
        // `maintain::reindex_if_stale`).
        crate::store::maintain::reindex_if_stale(&conn, cfg)?;
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
