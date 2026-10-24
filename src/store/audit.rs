//! RECORDING READS THAT CROSS A BOUNDARY (soma/ellmephant#129, the missing half of #120).
//!
//! The private tier decides who may see a row. It did not record when the decision let somebody
//! through — so an ancestor reading its subtree's private rows left no trace at all, and the module
//! that claimed "the audit is the point, not a courtesy" cited a table that did not exist.
//!
//! ★ BEST EFFORT, NEVER FATAL. A read must not fail because its record could not be written: the
//! alternative is that an audit outage becomes a read outage, which is a worse trade than a missing
//! line. Callers ignore the result; a failure is logged.
//!
//! ★ THE PEN IS A NAME. See the migration: an audit that dies with the registry audits the registry,
//! not the acts.

use rusqlite::{Connection, params};

use crate::error::Result;

/// Why a read crossed a boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// An ancestor read a descendant's private row.
    Subtree,
    /// A search returned rows private to a descendant.
    Search,
    /// An explicit exemption. **The capability is not built**; the reason is reserved so rows written
    /// later are unambiguous about which mechanism produced them.
    Forensic,
}

impl Why {
    /// The string stored in the row.
    pub fn as_str(self) -> &'static str {
        match self {
            Why::Subtree => "subtree",
            Why::Search => "search",
            Why::Forensic => "forensic",
        }
    }
}

/// Record one read that crossed a boundary. Best effort — see the module note.
pub fn record(conn: &Connection, pen: &str, key: Option<&str>, why: Why, detail: Option<&str>) {
    let written = conn.execute(
        "INSERT INTO private_read (pen, key, reason, detail) VALUES (?1, ?2, ?3, ?4)",
        params![pen, key, why.as_str(), detail],
    );
    if let Err(e) = written {
        tracing::warn!(pen = %pen, key = ?key, reason = why.as_str(), error = %e, "could not record a boundary read");
    }
}

/// One boundary read: `(at, pen, key, reason, detail)`.
pub type BoundaryRead = (String, String, Option<String>, String, Option<String>);

/// The most recent boundary reads, newest first.
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

/// How many boundary reads are on record.
pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM private_read", [], |r| r.get(0))?)
}

/// How long a boundary read is kept unless an operator says otherwise.
pub const KEEP_DAYS: i64 = 180;

/// Drop records older than `keep_days`. Retention is a decision — see the migration.
pub fn prune(conn: &Connection, keep_days: i64) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM private_read WHERE at < datetime('now', ?1)",
        params![format!("-{keep_days} days")],
    )?)
}
