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
use crate::embed::{Embedder, VecSpec, projection};
use crate::error::Result;
use crate::index;
use crate::store::{links, row_hash, tags};

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
    let spec = VecSpec::of(embedder);
    let rebuilt = index::vec::stored_dim(conn)? != Some(dim);
    if rebuilt {
        // A dimension change makes every stored vector the wrong width and therefore unreadable, so
        // the table is dropped and recreated. The one path that cannot be a swap, because there is
        // no old index worth keeping.
        index::vec::recreate(conn, dim)?;
    }

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
    let batches = rows_count.div_ceil(BATCH).max(1);
    // ONE transaction, and the clear is INSIDE it. The clear used to run before the tx opened,
    // which left `memory_vec` genuinely empty for the whole rebuild: `/health` reported `vectors: 0`
    // and `/search` answered an empty hit list at HTTP 200 while the store still held every memory.
    // A reader sees the OLD index until this commits, so a rebuild is slow, not broken.
    let tx = conn.unchecked_transaction()?;
    if !rebuilt {
        index::vec::clear(&tx)?; // full rebuild: drop any stale or orphaned vectors, atomically
    }
    meta::set(&tx, meta::DIM, &dim.to_string())?;
    meta::set(&tx, meta::MODEL, embedder.model_id())?;
    meta::set(&tx, meta::INSTRUCTION, instruction)?;
    // The vectors now correspond to the *current* projection, so this is the one place the
    // fingerprint legitimately moves ([`crate::index::vec::projection_stale`]).
    meta::set(&tx, meta::PROJECTION, &projection::fingerprint())?;

    let mut done = 0;
    for (batch, chunk) in rows.chunks(BATCH).enumerate() {
        let docs: Vec<String> = chunk
            .iter()
            .map(|(_, key, _tags, title, body)| projection::embed_document(key, title, body))
            .collect();
        let vectors = embedder.embed(&docs)?;
        for (i, (id, _, _, _, _)) in chunk.iter().enumerate() {
            index::vec::upsert(&tx, *id, &vectors[i])?;
            tx.execute(
                "UPDATE memory SET doc_hash = ?2 WHERE id = ?1",
                params![id, row_hash(&spec, &docs[i])],
            )?;
        }
        done += chunk.len();
        // A real rebuild runs for minutes and used to log nothing between "starting" and the reply,
        // so the only way to tell progress from a hang was to watch the CPU.
        tracing::info!(
            batch = batch + 1,
            batches,
            done,
            rows = rows_count,
            "reembed progress"
        );
    }
    tx.commit()?;

    // The link graph is derived too: re-parse every body, so a parser change — or a body written by
    // an older version — finally reaches the stored edges.
    let linked = links::reparse(conn)?;

    Ok(Report {
        rows: rows_count,
        dim,
        rebuilt,
        model: embedder.model_id().to_string(),
        linked,
    })
}
