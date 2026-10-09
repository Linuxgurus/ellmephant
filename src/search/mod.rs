//! Hybrid search orchestration: run both legs, fuse with RRF, hydrate rows (P5).
//!
//! The two legs answer different questions — BM25 finds exact tokens, KNN finds meaning —
//! and their scores are not comparable, so [`rrf::fuse`] merges them *by rank*. Every hit
//! carries its fused score (and vector distance when the KNN leg contributed) so the
//! caller can judge confidence (SPEC §10, §14).

pub mod rrf;

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::index;
use crate::store::{self, Memory};

/// Which retrieval legs to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// BM25 ∪ KNN, fused with RRF.
    #[default]
    Hybrid,
    /// KNN only.
    Vec,
    /// BM25 only.
    Fts,
}

impl Mode {
    /// Parse a CLI/config string (`hybrid`, `vec`, `fts`).
    pub fn parse(s: &str) -> Result<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "hybrid" => Ok(Mode::Hybrid),
            "vec" => Ok(Mode::Vec),
            "fts" => Ok(Mode::Fts),
            other => Err(crate::error::Error::Config(format!(
                "invalid mode {other:?} (want hybrid|vec|fts)"
            ))),
        }
    }

    /// Canonical name.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Hybrid => "hybrid",
            Mode::Vec => "vec",
            Mode::Fts => "fts",
        }
    }

    /// Whether this mode needs an embedder (i.e. the KNN leg runs).
    pub fn needs_embedder(self) -> bool {
        matches!(self, Mode::Hybrid | Mode::Vec)
    }
}

/// Search request.
#[derive(Debug, Clone)]
pub struct Query {
    /// The user's question.
    pub text: String,
    /// Restrict to one bucket.
    pub scope: Option<String>,
    /// Result count.
    pub k: usize,
    /// `hybrid` (default), `vec` or `fts`.
    pub mode: Mode,
    /// Optional cosine-distance ceiling.
    pub max_distance: Option<f32>,
    /// Link-neighbourhood hops to include (0 = off).
    pub expand: u8,
}

/// One hydrated hit. Scores are returned so the caller can judge confidence
/// (SPEC §14 false-memory hazard).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    /// `memory.id`.
    pub id: i64,
    /// Slug.
    pub key: String,
    /// Bucket.
    pub scope: String,
    /// Usage class.
    #[serde(rename = "type")]
    pub type_: String,
    /// Salient line.
    pub title: String,
    /// Full body (already bounded by the caller's snippet policy).
    pub body: String,
    /// Fused score.
    pub score: f64,
    /// Vector distance when the KNN leg contributed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
    /// Attached tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A link-neighbour pulled in by `--expand` (SPEC §10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Neighbor {
    /// Hops from the nearest seed (1 = directly linked).
    pub hop: u8,
    /// The key we travelled *from* to reach this row.
    pub via: String,
    /// The neighbour row.
    pub memory: Memory,
}

/// The result bundle: ranked hits plus (when asked) their link neighbourhood.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bundle {
    /// Ranked hits.
    pub hits: Vec<Hit>,
    /// Link neighbours, breadth-first from the hits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub neighbors: Vec<Neighbor>,
}

/// Run a query. `embedder` is required for `hybrid`/`vec` and ignored by `fts`.
pub fn run(conn: &Connection, embedder: Option<&dyn Embedder>, q: &Query) -> Result<Bundle> {
    let limit = q.k.max(1);
    let scope = q.scope.as_deref();

    let mut vec_hits: Vec<(i64, f32)> = Vec::new();
    if q.mode.needs_embedder() {
        let embedder = embedder.ok_or_else(|| {
            Error::Config(format!("{} search needs an embedder", q.mode.as_str()))
        })?;
        let query_vector = embedder.embed_query(&q.text)?;
        let hint = index::vec::Knn {
            k: limit,
            scope,
            max_distance: q.max_distance,
        };
        vec_hits = index::vec::knn(conn, &query_vector, &hint)?;
    }

    let fts_hits = if matches!(q.mode, Mode::Hybrid | Mode::Fts) {
        index::fts::search(conn, &q.text, scope, limit)?
    } else {
        Vec::new()
    };

    // RRF is rank-based, so the two score scales never meet (SPEC §10).
    let vec_ranking: Vec<i64> = vec_hits.iter().map(|(id, _)| *id).collect();
    let fts_ranking: Vec<i64> = fts_hits.iter().map(|h| h.id).collect();
    let rankings: Vec<Vec<i64>> = match q.mode {
        Mode::Vec => vec![vec_ranking],
        Mode::Fts => vec![fts_ranking],
        Mode::Hybrid => vec![vec_ranking, fts_ranking],
    };
    let fused = rrf::fuse(&rankings, rrf::DEFAULT_RRF_K);

    let distances: HashMap<i64, f32> = vec_hits.into_iter().collect();

    let mut hits = Vec::with_capacity(limit);
    for (id, score) in fused.into_iter().take(limit) {
        let Some(mem) = store::get_by_id(conn, id)? else {
            continue; // a leg returned an id that vanished; skip, don't fail the query
        };
        hits.push(Hit {
            id,
            key: mem.key,
            scope: mem.scope,
            type_: mem.type_.as_str().to_string(),
            title: mem.title,
            body: mem.body,
            score,
            distance: distances.get(&id).copied(),
            tags: mem.tags,
        });
    }

    let neighbors = if q.expand > 0 {
        expand(conn, &hits, q.expand)?
    } else {
        Vec::new()
    };
    Ok(Bundle { hits, neighbors })
}

/// Breadth-first over `memory_link` in *both* directions, up to `hops` from the seeds.
/// Dangling targets (a `[[key]]` to a row that does not exist) are silently skipped.
pub fn expand(conn: &Connection, seeds: &[Hit], hops: u8) -> Result<Vec<Neighbor>> {
    let mut visited: HashSet<i64> = seeds.iter().map(|h| h.id).collect();
    let mut frontier: Vec<(i64, String)> = seeds.iter().map(|h| (h.id, h.key.clone())).collect();
    let mut out = Vec::new();

    for hop in 1..=hops.max(1) {
        frontier = expand_frontier(conn, &frontier, hop, &mut visited, &mut out)?;
        if frontier.is_empty() {
            break;
        }
    }
    Ok(out)
}

/// One BFS level: append every unseen neighbour of `frontier` to `out` and return the
/// frontier for the next hop.
fn expand_frontier(
    conn: &Connection,
    frontier: &[(i64, String)],
    hop: u8,
    visited: &mut HashSet<i64>,
    out: &mut Vec<Neighbor>,
) -> Result<Vec<(i64, String)>> {
    let mut next = Vec::new();
    for (id, key) in frontier {
        for (nid, nkey) in adjacent(conn, *id, key)? {
            if !visited.insert(nid) {
                continue;
            }
            let Some(memory) = store::get_by_id(conn, nid)? else {
                continue;
            };
            out.push(Neighbor {
                hop,
                via: key.clone(),
                memory,
            });
            next.push((nid, nkey));
        }
    }
    Ok(next)
}

/// Neighbours of `(id, key)` in both directions: outgoing `[[…]]` edges and incoming
/// backlinks, as `(id, key)` pairs.
fn adjacent(conn: &Connection, id: i64, key: &str) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.key FROM memory_link l JOIN memory m ON m.key = l.to_key
          WHERE l.from_id = ?1
         UNION
         SELECT m.id, m.key FROM memory_link l JOIN memory m ON m.id = l.from_id
          WHERE l.to_key = ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![id, key], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
