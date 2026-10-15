//! Database open + schema migration. One writer connection; readers open their own.

pub mod meta;
pub mod migrate;

use std::time::Duration;

use rusqlite::Connection;

use tracing::{debug, info};

use crate::config::Paths;
use crate::error::Result;

/// Open (creating if needed) the store, apply pragmas, and run migrations.
///
/// Logs the store's identity at startup: whether the file was CREATED (a fresh store is the one
/// case where "where did my memories go?" has a benign answer) and what schema it ends up at. Both
/// are `info`/`debug` on a path that runs once per process, so a pod's log opens with the facts
/// needed to tell a new store from a migrated one.
pub fn open(paths: &Paths) -> Result<Connection> {
    paths.ensure()?;
    // Auto-extensions only attach to connections created *after* registration, so this
    // must run before `Connection::open` (PLAN §5 S0.1).
    crate::index::vec::register_extension();
    let fresh = !paths.db().exists();
    let conn = Connection::open(paths.db())?;
    if fresh {
        info!(db = %paths.db().display(), "created a new store");
    }
    configure(&conn)?;
    migrate::run(&conn)?;
    debug!(
        db = %paths.db().display(),
        schema = migrate::version(&conn)?,
        "store opened"
    );
    Ok(conn)
}

/// PRAGMAs from SPEC §7 / PLAN §2 #10. `journal_mode` returns a row, so it is
/// queried rather than executed.
pub fn configure(conn: &Connection) -> Result<()> {
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_millis(5_000))?;
    debug!(journal = %mode, busy_timeout_ms = 5_000, "store configured");
    Ok(())
}
