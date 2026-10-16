//! Rebuild the derived indexes from `memory` (PLAN §6 P9): the vector index and the `[[link]]`
//! graph.
//!
//! The relational rows are the source of truth, so changing the encoder (or its dimension)
//! is a *re-embed*, never a migration: walk `memory`, recompute the document projection,
//! rewrite each row's `encode_hash`, and repopulate `memory_vec`. The `[[link]]` graph is derived
//! too — its edges are computed at *write* time — so the same pass re-parses every body
//! ([`links::reparse`]), making "derived and rebuildable" true rather than aspirational. `meta`
//! records what the vectors now correspond to so `doctor` can tell a swapped model from a corrupt
//! one.

use rusqlite::{Connection, params};

use crate::db::meta;
use crate::embed::{VecSpec, projection};
use crate::error::Result;
use crate::index;
use crate::store::{ENCODE_PENDING, encode, links, row_hash};

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

    // A changed PROJECTION is a full re-embed too, and **`encode_hash` cannot see it**: the hash covers the
    // model, the dimension and the text — not the recipe that assembled them. The fingerprint is the
    // signal, and without it a projection change would queue nothing at all while every stored vector
    // quietly kept describing the old recipe. That is the drift the fingerprint exists to catch, and
    // this is the one place it is acted on.
    let reset = rebuilt || index::vec::projection_stale(conn)?;
    // Snapshot the rows first, so the scan is not interleaved with the enqueues.
    let rows: Vec<(i64, String, String, String, String)> = {
        let mut stmt =
            conn.prepare("SELECT id, key, title, body, encode_hash FROM memory ORDER BY id")?;
        let mapped = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // Which rows actually HAVE a vector, which is not what `encode_hash` records: that column says a
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
        let encode_hash = row_hash(spec, &projection::embed_document(&key, &title, &body));
        // A row is owed a vector when the table was rebuilt or the projection moved (every stored
        // vector is the wrong width, or describes the old recipe), when its document changed, or when
        // it has no vector at all. `encode_hash` cannot see the last two on its own — a killed rebuild
        // leaves the hash saying "indexed", and the projection was never part of the hash — and a torn
        // or drifted index is exactly what this function exists to repair.
        if reset || stored != encode_hash || !indexed.contains(&id) {
            encode::enqueue(&tx, &key, &encode_hash, encode::BACKFILL)?;
            if reset {
                // **Clear the hash, or the work never happens.** A reset means the stored vector was
                // built with a different recipe (or a different width), so it is not this row's vector
                // *under the current one* — and the worker would otherwise look at the still-matching
                // `encode_hash`, find the old vector present, decide `needs_embed = false`, and settle
                // the entry without embedding anything. The re-embed would report 280 queued and change
                // nothing at all, silently. `encode_hash = 'encode_pending'` is precisely the store
                // saying "no vector for this text", so the worker embeds.
                tx.execute(
                    "UPDATE memory SET encode_hash = ?2 WHERE id = ?1",
                    params![id, ENCODE_PENDING],
                )?;
            }
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
