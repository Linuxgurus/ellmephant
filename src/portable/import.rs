//! Read JSONL back into the store (P8).

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use rusqlite::Connection;

use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::index;
use crate::portable::Record;
use crate::store::{self, Memory};

/// Rows embedded per [`Embedder::embed`] batch.
const BATCH: usize = 16;

/// Upsert each canonical record by `key`, re-embedding only when the recomputed row
/// hash differs from what is already stored. Returns the number of records that were
/// new or changed, so re-importing the same file returns `0` (idempotent) and
/// `dry_run` becomes a count of pending changes (SPEC §11, PLAN §6 P8).
///
/// The whole file is parsed and validated before anything is written, and every write
/// lands in one transaction, so a bad record cannot leave a half-imported store.
pub fn from_jsonl(
    conn: &Connection,
    embedder: &dyn Embedder,
    path: &Path,
    dry_run: bool,
) -> Result<usize> {
    // Parse first: a bad or keyless record fails before any mutation.
    let memories = read_records(path)?;

    // Resolve every write (reads only) and collect the texts that need embedding.
    let mut prepared = Vec::with_capacity(memories.len());
    let mut pending: Vec<(usize, String)> = Vec::new();
    for (i, mem) in memories.iter().enumerate() {
        let plan = store::prepare(conn, embedder, mem)?;
        if plan.needs_embed() {
            pending.push((i, plan.text.clone()));
        }
        prepared.push(plan);
    }
    let changed = pending.len();
    if dry_run {
        return Ok(changed);
    }

    // Embed outside the transaction, one batch at a time (inference is the slow part).
    let mut vectors: Vec<Option<Vec<f32>>> = vec![None; memories.len()];
    for chunk in pending.chunks(BATCH) {
        let texts: Vec<String> = chunk.iter().map(|(_, text)| text.clone()).collect();
        let embedded = embedder.embed(&texts)?;
        for ((i, _), vector) in chunk.iter().zip(embedded) {
            vectors[*i] = Some(vector);
        }
    }

    // One transaction for the whole file: import is all-or-nothing.
    index::vec::ensure(conn, embedder.dim())?;
    let tx = conn.unchecked_transaction()?;
    for ((mem, plan), vector) in memories.iter().zip(&prepared).zip(&vectors) {
        store::apply(&tx, mem, plan, vector.as_deref())?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Read every non-empty line as a [`Record`] into a [`Memory`], naming the line on failure.
fn read_records(path: &Path) -> Result<Vec<Memory>> {
    let reader = BufReader::new(File::open(path)?);
    let mut out = Vec::new();
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
        out.push(Memory::from(record));
    }
    Ok(out)
}
