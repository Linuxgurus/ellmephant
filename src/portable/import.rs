//! Read JSONL back into the store (P8).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::embed::{Embedder, projection};
use crate::error::{Error, Result};
use crate::portable::Record;
use crate::store::{self, Memory, slug};

/// Upsert each canonical record by `key`, re-embedding only when the recomputed row
/// hash differs from what is already stored. Returns the number of records that were
/// new or changed, so re-importing the same file returns `0` (idempotent) and
/// `dry_run` becomes a count of pending changes (SPEC §11, PLAN §6 P8).
pub fn from_jsonl(
    conn: &Connection,
    embedder: &dyn Embedder,
    path: &Path,
    dry_run: bool,
) -> Result<usize> {
    let reader = BufReader::new(File::open(path)?);
    let mut changed = 0;
    for (line_no, line) in reader.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(line).map_err(|e| {
            Error::Config(format!(
                "{}:{}: invalid JSONL record: {e}",
                path.display(),
                line_no + 1
            ))
        })?;
        if record.key.trim().is_empty() {
            return Err(Error::Config(format!(
                "{}:{}: record is missing `key`",
                path.display(),
                line_no + 1
            )));
        }

        let mem = Memory::from(record);
        // `store::add` slugifies an explicit key, so compare against the same form.
        let key = slug::slugify(&mem.key);
        let hash = store::row_hash(embedder, &projection::embed_text(&mem.title, &mem.body));
        let existing: Option<String> = conn
            .query_row(
                "SELECT hash FROM memory WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        if existing.as_deref() != Some(hash.as_str()) {
            changed += 1;
        }
        if !dry_run {
            store::add(conn, embedder, &mem)?;
        }
    }
    Ok(changed)
}
