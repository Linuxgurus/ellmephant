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

/// `meta` key recording the last rebuild DECISION: the estimate before it started, or the refusal.
/// Answerable later because "the rebuild cost X" was unanswerable before (soma/ellmephant#76).
pub const REBUILD: &str = "rebuild";

/// Measured cost of one document embed, in seconds, at the slow end (5–11.5 s/doc — #62's rationale).
/// Machine-dependent, so an estimate is an ORDER, not a promise; it exists to refuse a rebuild that is
/// off by an order of magnitude, not to predict the minute it finishes.
pub const EST_SECONDS_PER_DOC: f64 = 11.5;

/// What a rebuild will cost, before anything is purged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// Documents that would be queued.
    pub documents: usize,
    /// Estimated embed time, rounded up to whole seconds.
    pub seconds: u64,
}

impl Estimate {
    /// Estimate a rebuild of `documents` documents.
    pub fn of(documents: usize) -> Self {
        Self {
            documents,
            seconds: (documents as f64 * EST_SECONDS_PER_DOC).ceil() as u64,
        }
    }
}

/// The shape of [`reindex_if_stale`]'s answer, because "nothing to do" and "refused" are different
/// facts and a bare `Option` made them the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebuild {
    /// The projection was current; nothing was touched.
    Current,
    /// The rebuild was queued (documents) — it proceeds.
    Started {
        /// Documents queued.
        documents: usize,
        /// The estimate recorded before the purge.
        estimate: Estimate,
    },
    /// The estimate exceeded the budget: the index is UNTOUCHED and stays STALE.
    Refused {
        /// What it would have cost.
        estimate: Estimate,
        /// The configured ceiling.
        budget: u64,
    },
}

/// A changed projection is a **purge and a full rebuild**, never a refill — and it happens at open.
/// The stamp this design deleted was per-row, so a stale row could only be found by comparing it to
/// something. `meta.PROJECTION` is a *global*, and a global cannot say which rows were built under
/// which recipe. The failure it hides, measured by stet (2026-10-21):
///
/// 1. the model changes and the fingerprint moves;
/// 2. a write lands before the rebuild — its text is new, so it enqueues and is embedded with the
///    **new** model, beside old-model vectors;
/// 3. a refill walks "what has no vector" and **skips that row, because it has one**.
///
/// Permanently mixed, and silent: the queue is empty, nothing names the rows, and the invariant
/// reports "current". Distances across two models are not slightly wrong, they are meaningless — a
/// mixed index ranks garbage confidently, which is worse than an empty one.
///
/// So projection change ⇒ **delete every vector, including the tombstones, and queue every document**,
/// all in ONE transaction with the fingerprint. Atomicity is the requirement, not a nicety: no read
/// sees a half-purged index, and no write can slip a new-model vector into an old-model index between
/// the detection and the purge. While the queue is non-empty the index is *incomplete*, which is
/// honest; `/health` and `doctor` report the non-empty queue as REBUILDING
/// ([`crate::store::encode::owed`]).
///
/// ★ AND THE BUDGET IS ASKED FIRST (soma/ellmephant#76). The purge is milliseconds and the rebuild is
/// an hour; a config change must not commit the store to work nobody priced. If the estimate exceeds
/// `cfg.rebuild_max_documents`, NOTHING is deleted and the fingerprint is NOT moved — the store stays
/// STALE, which is the state this design already has a name for, and the encoder refuses to embed into
/// a mixed index (see [`crate::mcp`]'s worker). Recovery is a decision: raise the budget, roll the
/// config back, or run `reembed` by hand — never a silent half-index.
pub fn reindex_if_stale(conn: &Connection, cfg: &crate::config::Config) -> Result<Option<usize>> {
    Ok(match reindex(conn, cfg)? {
        Rebuild::Started { documents, .. } => Some(documents),
        Rebuild::Current | Rebuild::Refused { .. } => None,
    })
}

/// The decision and the rebuild in one place, so the caller can say WHAT happened.
pub fn reindex(conn: &Connection, cfg: &crate::config::Config) -> Result<Rebuild> {
    if !index::vec::projection_stale(conn)? {
        return Ok(Rebuild::Current);
    }
    let rows: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare("SELECT key, title, body FROM memory ORDER BY id")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let estimate = Estimate::of(rows.len());

    if !cfg.rebuild_allow_over_budget && rows.len() as u64 > cfg.rebuild_max_documents {
        // ★ NOTHING IS PURGED. The decision is recorded where a human will find it, and the old
        // index keeps answering — stale, said out loud by `projection_stale`, never mixed.
        meta::set(
            conn,
            REBUILD,
            &format!(
                "REFUSED docs={} est_s={} budget={} at={}",
                estimate.documents,
                estimate.seconds,
                cfg.rebuild_max_documents,
                now_epoch(conn)?
            ),
        )?;
        tracing::error!(
            documents = estimate.documents,
            est_seconds = estimate.seconds,
            budget = cfg.rebuild_max_documents,
            "index: rebuild REFUSED — the estimate exceeds `rebuild_max_documents`; the index is \
             UNTOUCHED and stays STALE. Raise the budget deliberately, or roll the config back."
        );
        return Ok(Rebuild::Refused {
            estimate,
            budget: cfg.rebuild_max_documents,
        });
    }

    let started = now_epoch(conn)?;
    let tx = conn.unchecked_transaction()?;
    index::vec::clear_all(&tx)?;
    meta::set(&tx, meta::PROJECTION, &projection::fingerprint())?;
    // The estimate, recorded in the SAME transaction as the purge: a rebuild that exists without a
    // price on the books is the thing this issue is about.
    meta::set(
        &tx,
        REBUILD,
        &format!(
            "docs={} est_s={} budget={} started={started}",
            estimate.documents, estimate.seconds, cfg.rebuild_max_documents
        ),
    )?;
    for (key, title, body) in &rows {
        encode::enqueue_memory(&tx, key, title, body, encode::PRIORITY_BULK)?;
    }
    tx.commit()?;
    tracing::warn!(
        "index: projection changed — purged every vector (tombstones included) and queued {} document(s) for a full rebuild (est {}s at {}s/doc)",
        rows.len(),
        estimate.seconds,
        EST_SECONDS_PER_DOC
    );
    Ok(Rebuild::Started {
        documents: rows.len(),
        estimate,
    })
}

/// Close the books when the last owed vector settles: the ACTUAL duration, next to the estimate.
///
/// Called by the encode worker when its queue drains (see `mcp::pass`). Idempotent: a rebuild that is
/// already marked finished is left alone, so a later unrelated drain never re-stamps it.
pub fn note_rebuild_finished(conn: &Connection) -> Result<()> {
    let Some(record) = meta::get(conn, REBUILD)? else {
        return Ok(());
    };
    if record.contains("finished=") {
        return Ok(());
    }
    let Some(started) = record
        .split_whitespace()
        .find_map(|f| f.strip_prefix("started="))
        .and_then(|v| v.parse::<i64>().ok())
    else {
        return Ok(());
    };
    let now = now_epoch(conn)?;
    meta::set(
        conn,
        REBUILD,
        &format!(
            "{record} finished={now} elapsed_s={}",
            (now - started).max(0)
        ),
    )?;
    tracing::info!(
        elapsed_s = (now - started).max(0),
        "index: rebuild finished — the estimate and the actual are both on the books"
    );
    Ok(())
}

/// Epoch seconds from the store, so timestamps come from one clock rather than the process's.
fn now_epoch(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT CAST(strftime('%s','now') AS INTEGER)", [], |r| {
            r.get(0)
        })?,
    )
}

/// What a link re-parse did. Model-free and seconds long, so it is reported synchronously rather
/// than queued.
#[derive(Debug, Clone)]
pub struct Reparse {
    /// Memories whose `[[link]]` edges were re-derived from their body.
    pub rows: usize,
    /// Edges the graph holds afterwards — the number a re-parse is judged against `stats.dangling`.
    pub edges: usize,
}

/// Rebuild the `[[link]]` graph from the current bodies — NO model, NO queue.
///
/// `memory_link` is derived, but its edges are computed at *write* time, so a body rewritten by
/// anything that does not re-derive (a textual rewrite, a parser change, an older binary) leaves a
/// graph the rows no longer imply. Re-embedding would fix it and costs an hour of worker time and a
/// model load; this costs milliseconds ([`links::reparse`]).
pub fn reparse(conn: &Connection) -> Result<Reparse> {
    let rows = links::reparse(conn)?;
    let edges: i64 = conn.query_row("SELECT COUNT(*) FROM memory_link", [], |r| r.get(0))?;
    Ok(Reparse {
        rows,
        edges: edges.max(0) as usize,
    })
}

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
