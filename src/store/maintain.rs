//! Rebuild the derived vector index from `memory` (PLAN §6 P9).
//!
//! The relational rows are the source of truth, so changing the encoder (or its dimension)
//! is a *re-embed*, never a migration: walk `memory`, recompute each projection, rewrite the
//! per-row `hash`/`model`/`dim`, and repopulate `memory_vec`. `meta` records what the vectors
//! now correspond to so `doctor` can tell a swapped model from a corrupt one.

use rusqlite::{Connection, params};

use crate::db::meta;
use crate::embed::{Embedder, projection};
use crate::error::Result;
use crate::index;
use crate::store::row_hash;

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

/// Recompute every vector and rebuild `memory_vec` (PLAN §6 P9). Handles a dimension change
/// by dropping and recreating the table (the one path [`index::vec::ensure`] refuses), and
/// records `dim`/`model`/`instruction` in `meta`.
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
    let rows: Vec<(i64, String, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, scope, title, body FROM memory ORDER BY id")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let rows_count = rows.len();
    let dim_col = dim as i64;
    let tx = conn.unchecked_transaction()?;
    for chunk in rows.chunks(BATCH) {
        let texts: Vec<String> = chunk
            .iter()
            .map(|(_, _, title, body)| projection::embed_text(title, body))
            .collect();
        let vectors = embedder.embed(&texts)?;
        for ((id, scope, _, _), (text, vector)) in chunk.iter().zip(texts.iter().zip(&vectors)) {
            index::vec::upsert(&tx, *id, scope, vector)?;
            tx.execute(
                "UPDATE memory SET hash = ?2, model = ?3, dim = ?4 WHERE id = ?1",
                params![id, row_hash(embedder, text), embedder.model_id(), dim_col],
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
