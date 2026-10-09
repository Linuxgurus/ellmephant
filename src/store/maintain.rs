//! Rebuild the derived vector index from `memory` (PLAN §6 P9).
//!
//! The relational rows are the source of truth, so changing the encoder (or its dimension)
//! is a *re-embed*, never a migration: walk `memory`, recompute the document projection,
//! rewrite each row's `doc_hash`, and repopulate `memory_vec`. `meta` records what the
//! vectors now correspond to so `doctor` can tell a swapped model from a corrupt one.

use rusqlite::{Connection, params};

use crate::db::meta;
use crate::embed::{Embedder, projection};
use crate::error::Result;
use crate::index;
use crate::store::{row_hash, tags};

/// Rows embedded per `Embedder::embed` batch.
const BATCH: usize = 16;

/// What a re-embed did.
#[derive(Debug, Clone)]
pub struct Report {
    /// Rows re-embedded.
    pub rows: usize,
    /// The (possibly new) vector dimension.
    pub dim: usize,
    /// Whether the `vec0` table was recreated because the dimension changed.
    pub rebuilt: bool,
    /// The model id the vectors now correspond to.
    pub model: String,
}

/// Recompute every vector and rebuild the vec0 table (PLAN §6 P9). Handles a dimension
/// change by dropping and recreating the table (the one path [`index::vec::ensure`]
/// refuses), and records `dim`/`model`/`instruction` in `meta`.
pub fn reembed(conn: &Connection, embedder: &dyn Embedder, instruction: &str) -> Result<Report> {
    let dim = embedder.dim();
    let rebuilt = index::vec::stored_dim(conn)? != Some(dim);
    if rebuilt {
        index::vec::recreate(conn, dim)?;
    } else {
        index::vec::clear(conn)?; // full rebuild: drop any stale or orphaned vectors
    }
    meta::set(conn, meta::DIM, &dim.to_string())?;
    meta::set(conn, meta::MODEL, embedder.model_id())?;
    meta::set(conn, meta::INSTRUCTION, instruction)?;

    // Snapshot the rows to embed before opening the write transaction.
    let rows: Vec<(i64, String, Vec<String>, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, key, tags, title, body FROM memory ORDER BY id")?;
        let mapped = stmt.query_map([], |r| {
            let tags: String = r.get(2)?;
            Ok((
                r.get(0)?,
                r.get(1)?,
                tags::decode(&tags),
                r.get(3)?,
                r.get(4)?,
            ))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let rows_count = rows.len();
    // The batch embed stays inside the tx on purpose: a full rebuild wants one atomic swap
    // of the whole index, and it is a deliberate, rare maintenance command.
    let tx = conn.unchecked_transaction()?;
    for chunk in rows.chunks(BATCH) {
        let docs: Vec<String> = chunk
            .iter()
            .map(|(_, key, tags, title, body)| projection::embed_document(key, tags, title, body))
            .collect();
        let vectors = embedder.embed(&docs)?;
        for (i, (id, _, _, _, _)) in chunk.iter().enumerate() {
            index::vec::upsert(&tx, *id, &vectors[i])?;
            tx.execute(
                "UPDATE memory SET doc_hash = ?2 WHERE id = ?1",
                params![id, row_hash(embedder, &docs[i])],
            )?;
        }
    }
    tx.commit()?;

    Ok(Report {
        rows: rows_count,
        dim,
        rebuilt,
        model: embedder.model_id().to_string(),
    })
}
