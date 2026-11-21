use rusqlite::Connection;

use crate::db::meta;
use crate::embed::{VecSpec, projection};
use crate::error::Result;
use crate::index;
use crate::store::{encode, links};

pub const REBUILD: &str = "rebuild";

pub const EST_SECONDS_PER_DOC: f64 = 11.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub documents: usize,
    pub seconds: u64,
}

impl Estimate {
    pub fn of(documents: usize) -> Self {
        Self {
            documents,
            seconds: (documents as f64 * EST_SECONDS_PER_DOC).ceil() as u64,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebuild {
    Current,
    Started {
        documents: usize,
        estimate: Estimate,
    },
    Refused {
        estimate: Estimate,
        budget: u64,
    },
}

pub fn reindex_if_stale(conn: &Connection, cfg: &crate::config::Config) -> Result<Option<usize>> {
    Ok(match reindex(conn, cfg)? {
        Rebuild::Started { documents, .. } => Some(documents),
        Rebuild::Current | Rebuild::Refused { .. } => None,
    })
}

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

fn now_epoch(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT CAST(strftime('%s','now') AS INTEGER)", [], |r| {
            r.get(0)
        })?,
    )
}

#[derive(Debug, Clone)]
pub struct Reparse {
    pub rows: usize,
    pub edges: usize,
}

pub fn reparse(conn: &Connection) -> Result<Reparse> {
    let rows = links::reparse(conn)?;
    let edges: i64 = conn.query_row("SELECT COUNT(*) FROM memory_link", [], |r| r.get(0))?;
    Ok(Reparse {
        rows,
        edges: edges.max(0) as usize,
    })
}

#[derive(Debug, Clone)]
pub struct Report {
    pub queued: usize,
    pub dim: usize,
    pub rebuilt: bool,
    pub model: String,
    pub linked: usize,
}

pub fn reembed(conn: &Connection, spec: &VecSpec, instruction: &str) -> Result<Report> {
    let dim = spec.dim;
    let rebuilt = index::vec::stored_dim(conn)? != Some(dim);
    if rebuilt {
        index::vec::recreate(conn, dim)?;
    }

    let _was_stale = rebuilt || index::vec::projection_stale(conn)?;
    let rows: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare("SELECT key, title, body FROM memory ORDER BY id")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let tx = conn.unchecked_transaction()?;
    meta::set(&tx, meta::DIM, &dim.to_string())?;
    meta::set(&tx, meta::MODEL, &spec.model_id)?;
    meta::set(&tx, meta::INSTRUCTION, instruction)?;
    meta::set(&tx, meta::PROJECTION, &projection::fingerprint())?;

    let mut queued = 0;
    for (key, title, body) in rows {
        encode::enqueue_memory(&tx, &key, &title, &body, encode::PRIORITY_BULK)?;
        queued += 1;
    }
    tx.commit()?;

    let linked = links::reparse(conn)?;

    Ok(Report {
        queued,
        dim,
        rebuilt,
        model: spec.model_id.clone(),
        linked,
    })
}
