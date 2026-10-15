//! Rebuild the derived indexes from `memory` (PLAN §6 P9): the vector index and the `[[link]]`
//! graph.
//!
//! The relational rows are the source of truth, so changing the encoder (or its dimension)
//! is a *re-embed*, never a migration: walk `memory`, recompute the document projection,
//! rewrite each row's `doc_hash`, and repopulate `memory_vec`. The `[[link]]` graph is derived
//! too — its edges are computed at *write* time — so the same pass re-parses every body
//! ([`links::reparse`]), making "derived and rebuildable" true rather than aspirational. `meta`
//! records what the vectors now correspond to so `doctor` can tell a swapped model from a corrupt
//! one.

use rusqlite::{Connection, params};

use crate::db::meta;
use crate::embed::{Embedder, projection};
use crate::error::Result;
use crate::index;
use crate::store::{links, row_hash, tags};
use tracing::info;

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
    /// Memories whose `[[link]]` graph was re-parsed from its body.
    pub linked: usize,
}

/// Recompute every vector and rebuild the vec0 table (PLAN §6 P9). Handles a dimension
/// change by dropping and recreating the table (the one path [`index::vec::ensure`]
/// refuses), and records `dim`/`model`/`instruction` in `meta`.
///
/// Also re-parses every body into `memory_link` ([`links::reparse`]): that graph is derived from
/// the rows too, so a full rebuild is where a parser change (or an older body) catches up.
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
    // The vectors now correspond to the *current* projection, so this is the one place the
    // fingerprint legitimately moves ([`crate::index::vec::projection_stale`]).
    meta::set(conn, meta::PROJECTION, &projection::fingerprint())?;

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
    //
    // The progress line below is `info` on purpose, and it is the point of instrumenting this at
    // all. A rebuild is RARE, DELIBERATE and LONG -- the one operation anyone actually waits on --
    // while `doctor` can only ever report 0/N or N/N, because every row is written inside the single
    // uncommitted transaction. Without a per-batch line, "slow" and "stuck" look identical from
    // outside, which is exactly the hole this path used to have: no logging whatsoever.
    let batches = rows.len().div_ceil(BATCH);
    info!(
        rows = rows_count,
        batch = BATCH,
        batches,
        dim,
        model = embedder.model_id(),
        "reembed starting"
    );
    let started = std::time::Instant::now();
    let tx = conn.unchecked_transaction()?;
    for (n, chunk) in rows.chunks(BATCH).enumerate() {
        let docs: Vec<String> = chunk
            .iter()
            .map(|(_, key, _tags, title, body)| projection::embed_document(key, title, body))
            .collect();
        let vectors = embedder.embed(&docs)?;
        for (i, (id, _, _, _, _)) in chunk.iter().enumerate() {
            index::vec::upsert(&tx, *id, &vectors[i])?;
            tx.execute(
                "UPDATE memory SET doc_hash = ?2 WHERE id = ?1",
                params![id, row_hash(embedder, &docs[i])],
            )?;
        }
        let done = ((n + 1) * BATCH).min(rows_count);
        let elapsed = started.elapsed();
        info!(
            batch = n + 1,
            batches,
            rows_done = done,
            elapsed_ms = elapsed.as_millis() as u64,
            rate_rows_s = done as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
            "reembed progress"
        );
    }
    tx.commit()?;

    // The link graph is derived too: re-parse every body, so a parser change — or a body written by
    // an older version — finally reaches the stored edges.
    let linked = links::reparse(conn)?;

    info!(
        rows = rows_count,
        linked,
        rebuilt,
        elapsed_ms = started.elapsed().as_millis() as u64,
        dim,
        model = embedder.model_id(),
        "reembed complete"
    );

    Ok(Report {
        rows: rows_count,
        dim,
        rebuilt,
        model: embedder.model_id().to_string(),
        linked,
    })
}
