//! Write the store out as JSONL (P8).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::portable::Record;
use crate::store;

/// Stream every `memory` row to `path` as one canonical JSON object per line, ordered
/// by `key` so two exports of the same content are byte-identical. Returns the row count.
pub fn to_jsonl(conn: &Connection, path: &Path) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT id FROM memory ORDER BY key")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out = BufWriter::new(File::create(path)?);
    for id in &ids {
        let mem = store::get_by_id(conn, *id)?
            .ok_or_else(|| Error::NotFound(format!("memory id {id}")))?;
        serde_json::to_writer(&mut out, &Record::from(mem))?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(ids.len())
}
