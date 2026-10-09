//! The `meta` key/value table: bookkeeping for the *derived* indexes (`dim`, `model`,
//! `instruction`) so a rebuild can tell what the stored vectors were computed with.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

/// Vector dimensionality of the `vec0` index (frozen at `CREATE`).
pub const DIM: &str = "dim";
/// Embedder id the stored vectors correspond to.
pub const MODEL: &str = "model";
/// Query-side instruction the stored vectors were built with.
pub const INSTRUCTION: &str = "instruction";

/// Read one `meta` value.
pub fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT v FROM meta WHERE k = ?1", params![key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?)
}

/// Upsert one `meta` value.
pub fn set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (k, v) VALUES (?1, ?2)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![key, value],
    )?;
    Ok(())
}
