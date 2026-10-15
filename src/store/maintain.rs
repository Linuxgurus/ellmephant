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

use rusqlite::Connection;

use crate::db::meta;
use crate::embed::{VecSpec, projection};
use crate::error::Result;
use crate::index;
use crate::store::{encode, links, row_hash};

/// What a re-embed SCHEDULED. The work itself is done later, by the encode worker.
#[derive(Debug, Clone)]
pub struct Report {
    /// Documents queued for embedding.
    pub queued: usize,
    /// The (possibly new) vector dimension.
    pub dim: usize,
    /// Whether the `vec0` table was rebuilt because the dimension changed.
    pub rebuilt: bool,
    /// The model id the queued vectors will belong to.
    pub model: String,
    /// Memories whose `[[link]]` graph was re-parsed from its body.
    pub linked: usize,
}

/// Queue every document that needs (re-)embedding (PLAN §6 P9), and rebuild the derived parts that
/// take no model: the `[[link]]` graph, and the `vec0` table itself when the width changed.
///
/// **A producer, not an embedder.** It used to recompute every vector inside one transaction, holding
/// the store lock from the first row to the last — measured at ~45 minutes on a 291-row store, with
/// every write timing out for the duration while `/health` still said `ready`
/// (`ENCODE-WORKER.md`). Now it writes the `meta` facts, enqueues the work at
/// [`encode::BACKFILL`] priority so a live edit still wins the queue, and returns. The worker does the
/// rest, one document at a time.
pub fn reembed(conn: &Connection, spec: &VecSpec, instruction: &str) -> Result<Report> {
    let dim = spec.dim;
    let rebuilt = index::vec::stored_dim(conn)? != Some(dim);
    if rebuilt {
        // A dimension change makes every stored vector the wrong width and therefore unreadable, so
        // the table is dropped and recreated. The one step that cannot be a swap, and the only one
        // that has to happen *before* anything is queued.
        index::vec::recreate(conn, dim)?;
    }

    // Snapshot the rows first, so the scan is not interleaved with the enqueues.
    let rows: Vec<(i64, String, String, String, String)> = {
        let mut stmt =
            conn.prepare("SELECT id, key, title, body, doc_hash FROM memory ORDER BY id")?;
        let mapped = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // Which rows actually HAVE a vector, which is not what `doc_hash` records: that column says a
    // vector was once written for this text, and it survives a torn index.
    let indexed: std::collections::HashSet<i64> = index::vec::ids(conn)?.into_iter().collect();

    let tx = conn.unchecked_transaction()?;
    meta::set(&tx, meta::DIM, &dim.to_string())?;
    meta::set(&tx, meta::MODEL, &spec.model_id)?;
    meta::set(&tx, meta::INSTRUCTION, instruction)?;
    // The vectors are about to correspond to the *current* projection, so this is the one place the
    // fingerprint legitimately moves ([`crate::index::vec::projection_stale`]) — and the store is only
    // truly back in step once the worker's last job lands.
    meta::set(&tx, meta::PROJECTION, &projection::fingerprint())?;

    let mut queued = 0;
    for (id, key, title, body, stored) in rows {
        let doc_hash = row_hash(spec, &projection::embed_document(&key, &title, &body));
        // A row is owed a vector when the table was rebuilt (every stored vector is the wrong width),
        // when its document moved, or when it has no vector at all. `doc_hash` cannot see that last
        // case — it reports "indexed" precisely because a vector was once written — and a torn index
        // (a killed rebuild, a hand-deleted vector) is exactly what this function exists to repair.
        if rebuilt || stored != doc_hash || !indexed.contains(&id) {
            encode::enqueue(&tx, &key, &doc_hash, encode::BACKFILL)?;
            queued += 1;
        }
    }
    tx.commit()?;

    // The link graph is derived from the bodies too, so a full rebuild is where a parser change — or a
    // body written by an older version — finally reaches the stored edges. No model involved.
    let linked = links::reparse(conn)?;

    Ok(Report {
        queued,
        dim,
        rebuilt,
        model: spec.model_id.clone(),
        linked,
    })
}
