use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

pub const DIM: &str = "dim";
pub const MODEL: &str = "model";
pub const INSTRUCTION: &str = "instruction";
pub const PROJECTION: &str = "projection";

pub fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT v FROM meta WHERE k = ?1", params![key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?)
}

pub fn set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (k, v) VALUES (?1, ?2)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![key, value],
    )?;
    Ok(())
}
