use rusqlite::{Connection, params};

use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Subtree,
    Search,
    Forensic,
    Borrowed,
}

impl Why {
    pub fn as_str(self) -> &'static str {
        match self {
            Why::Subtree => "subtree",
            Why::Search => "search",
            Why::Forensic => "forensic",
            Why::Borrowed => "borrowed",
        }
    }
}

pub fn record(conn: &Connection, pen: &str, key: Option<&str>, why: Why, detail: Option<&str>) {
    let written = conn.execute(
        "INSERT INTO private_read (pen, key, reason, detail) VALUES (?1, ?2, ?3, ?4)",
        params![pen, key, why.as_str(), detail],
    );
    if let Err(e) = written {
        tracing::warn!(pen = %pen, key = ?key, reason = why.as_str(), error = %e, "could not record a boundary read");
    }
}

pub type BoundaryRead = (String, String, Option<String>, String, Option<String>);

pub fn recent(conn: &Connection, limit: i64) -> Result<Vec<BoundaryRead>> {
    let mut stmt = conn.prepare(
        "SELECT at, pen, key, reason, detail FROM private_read ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM private_read", [], |r| r.get(0))?)
}

pub const KEEP_DAYS: i64 = 180;

pub fn prune(conn: &Connection, keep_days: i64) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM private_read WHERE at < datetime('now', ?1)",
        params![format!("-{keep_days} days")],
    )?)
}
