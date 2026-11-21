pub mod meta;
pub mod migrate;

use std::time::Duration;

use rusqlite::Connection;

use crate::config::Paths;
use crate::error::Result;

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
    conn.busy_timeout(Duration::from_millis(5_000))?;
    Ok(conn)
}

pub fn configure(conn: &Connection) -> Result<()> {
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(Duration::from_millis(5_000))?;
    Ok(())
}
