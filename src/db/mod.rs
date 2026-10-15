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
