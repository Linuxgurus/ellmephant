//! Rebuild the derived indexes from `memory` (PLAN §6 P9): the vector index and the `[[link]]`
//! graph.
//!
//! The relational rows are the source of truth, so changing the encoder (or its dimension)
//! is a *re-embed*, never a migration: walk `memory`, recompute the document projection,
//! queue every document at bulk priority, and rebuild the `[[link]]` graph, which is derived
//! too — its edges are computed at *write* time — so the same pass re-parses every body
//! ([`links::reparse`]), making "derived and rebuildable" true rather than aspirational. `meta`
//! records what the vectors now correspond to so `doctor` can tell a swapped model from a corrupt
//! one.

use rusqlite::Connection;

use crate::db::meta;
use crate::embed::{VecSpec, projection};
use crate::error::Result;
use crate::index;
use crate::store::{encode, links};

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

    // ★ A changed PROJECTION is a full re-embed, and the fingerprint is what says so — but it no
    // longer decides *whether* anything is queued: a rebuild is always-full (see below), because with
    // no per-row stamp there is nothing to compare a row against. `projection_stale` is still read
    // below by `/health` and `doctor`, and the new fingerprint is recorded here.
    let _was_stale = rebuilt || index::vec::projection_stale(conn)?;
    // Snapshot the rows first, so the scan is not interleaved with the enqueues.
    let rows: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare("SELECT key, title, body FROM memory ORDER BY id")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let tx = conn.unchecked_transaction()?;
    meta::set(&tx, meta::DIM, &dim.to_string())?;
    meta::set(&tx, meta::MODEL, &spec.model_id)?;
    meta::set(&tx, meta::INSTRUCTION, instruction)?;
    // The vectors are about to correspond to the *current* projection, so this is the one place the
    // fingerprint legitimately moves ([`crate::index::vec::projection_stale`]) — and the store is only
    // truly back in step once the worker's last job lands.
    meta::set(&tx, meta::PROJECTION, &projection::fingerprint())?;

    // **Every** document, at bulk priority: a rebuild is not selective any more. With no per-row
    // stamp there is no "nothing changed" to skip, and adding one would be the stamp this design
    // deletes. What replaces the skip is the priority: a live write sorts ahead of every row here, so
    // a full re-encode cannot hold up an edit — and the index keeps answering throughout, because
    // nothing is deleted until (and unless) a dimension change forces it.
    let mut queued = 0;
    for (key, title, body) in rows {
        encode::enqueue_memory(&tx, &key, &title, &body, encode::PRIORITY_BULK)?;
        queued += 1;
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
