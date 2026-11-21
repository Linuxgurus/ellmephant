pub mod meta;
pub mod migrate;

use std::time::Duration;

use rusqlite::Connection;

use crate::config::Paths;
use crate::error::Result;

// ★ 30s, NOT 5s (2026-11-21): the claims race test starved a 5s busy window in the CI container and
// failed as `DatabaseBusy` on code that was correct — 2,000 rounds from two connections, one core
// short. Waiting is the right answer for a writer that is merely outranked; an error is only honest
// when the wait would outlast the caller's patience, and no write in this store holds the lock for
// anything close to 30s.
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

pub fn open(paths: &Paths) -> Result<Connection> {
    let cfg = crate::config::Config::load(paths)?;
    open_with(paths, &cfg)
}

pub fn open_with(paths: &Paths, cfg: &crate::config::Config) -> Result<Connection> {
    paths.ensure()?;
    crate::index::vec::register_extension();
    let conn = Connection::open(paths.db())?;
    configure(&conn)?;
    migrate::run(&conn)?;
    crate::store::sudo::load_at_boot(&conn);
    crate::store::load_tree_at_boot(&conn);
    crate::store::ensure_name_uniqueness(&conn)?;
    let _ = crate::store::mail::import_env_surrogates(&conn);
    let _ = crate::store::credential::import_env(&conn);
    let _ = crate::store::audit::prune(&conn, crate::store::audit::KEEP_DAYS);
    if let Some(dim) = crate::index::vec::stored_dim(&conn)? {
        crate::index::vec::ensure(&conn, dim)?;
        crate::store::maintain::reindex_if_stale(&conn, cfg)?;
    }
    Ok(conn)
}

pub fn open_read_only(paths: &Paths) -> Result<Connection> {
    crate::index::vec::register_extension();
    let conn = Connection::open_with_flags(paths.db(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

pub fn configure(conn: &Connection) -> Result<()> {
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}
