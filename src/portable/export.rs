//! Write the store out as JSONL (P8).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use rusqlite::Connection;

use crate::error::Result;
use crate::portable::Record;
use crate::store;

/// Write every `memory` row to `path` as one canonical JSON object per line, ordered
/// by `key` so two exports of the same content are byte-identical. Returns the row count.
///
/// Loads the store in two queries and writes via `<path>.tmp` + rename, so a failed
/// export never leaves a torn dump on disk.
pub fn to_jsonl(conn: &Connection, path: &Path) -> Result<usize> {
    let memories = store::all(conn)?;
    let count = memories.len();

    let tmp = path.with_extension("tmp");
    let mut out = BufWriter::new(File::create(&tmp)?);
    for mem in memories {
        serde_json::to_writer(&mut out, &Record::from(mem))?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    out.get_ref().sync_all()?;
    drop(out);
    std::fs::rename(&tmp, path)?;
    Ok(count)
}
