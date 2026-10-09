//! Search: a single semantic phrase over the store (SPEC §10).
//!
//! A query is one `phrase`, embedded and compared against the stored document embeddings by
//! cosine distance. There are no key/tag constraints and no literal (regex) leg — the key and
//! tags are part of the embedded document, so a query reaches them semantically.
//!
//! Results are distance-ranked, capped by `k` and a `max_distance` ceiling. A hit carries
//! `key`/`title`/`tags` and its `distance` — never a body; fetch it with `memory_get`/`show`.

/// Characters of a neighbour's body returned by `--expand` (SPEC §9/§14: snippets, not dumps).
pub const SNIPPET_CHARS: usize = 280;

/// First `max` characters of `s`, with a trailing `…` when it was truncated.
pub fn snippet(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

use std::collections::HashSet;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::index;
use crate::store::{self, Memory};

/// A candidate row: its id and, when the phrase was semantic, its cosine distance.
type Candidate = (i64, Option<f32>);

/// Search request — a single semantic phrase and its ranking knobs (SPEC §10).
#[derive(Debug, Clone)]
pub struct Query {
    /// The phrase; `None`/blank means "no phrase" (every row, key order).
    pub text: Option<String>,
    /// Result cap (semantic ranking only).
    pub k: usize,
    /// Optional cosine-distance ceiling.
    pub max_distance: Option<f32>,
    /// Link-neighbourhood hops to include (0 = off).
    pub expand: u8,
}

/// One hydrated hit: `key`/`title`/`tags` and (semantic only) `distance`. No body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    /// `memory.id` (local; not serialized).
    #[serde(skip)]
    pub id: i64,
    /// Slug.
    pub key: String,
    /// Salient line.
    pub title: String,
    /// Attached tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Vector distance when the semantic leg ranked this hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
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

/// The result bundle: hits plus (when asked) their link neighbourhood.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bundle {
    /// Hits, distance-ranked when the phrase was semantic.
    pub hits: Vec<Hit>,
    /// Link neighbours, breadth-first from the hits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub neighbors: Vec<Neighbor>,
}

/// Run a query. `embedder` is required when a phrase is given, ignored otherwise.
pub fn run(conn: &Connection, embedder: Option<&dyn Embedder>, q: &Query) -> Result<Bundle> {
    let phrase = phrase_ids(conn, embedder, q)?;
    let ranked = phrase.is_some();

    let mut ordered: Vec<(i64, Option<f32>)> = match phrase {
        Some(p) => p,
        None => all_ranked(conn)?,
    };
    if ranked {
        ordered.truncate(q.k.max(1)); // k caps the distance-ranked case only
    }

    let mut hits = Vec::with_capacity(ordered.len());
    for (id, distance) in ordered {
        let Some(mem) = store::get_by_id(conn, id)? else {
            continue; // the leg returned an id that vanished; skip, don't fail the query
        };
        hits.push(Hit {
            id,
            key: mem.key,
            title: mem.title,
            tags: mem.tags,
            distance,
        });
    }

    let neighbors = if q.expand > 0 {
        expand(conn, &hits, q.expand)?
    } else {
        Vec::new()
    };
    Ok(Bundle { hits, neighbors })
}

/// Every id, `key` order — the "no constraints at all" case.
fn all_ranked(conn: &Connection) -> Result<Vec<(i64, Option<f32>)>> {
    let mut stmt = conn.prepare("SELECT id FROM memory ORDER BY key")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push((row?, None));
    }
    Ok(out)
}

/// Candidates satisfying the phrase, distance-ranked (`None` when no phrase given).
fn phrase_ids(
    conn: &Connection,
    embedder: Option<&dyn Embedder>,
    q: &Query,
) -> Result<Option<Vec<Candidate>>> {
    let Some(text) = q.text.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let embedder =
        embedder.ok_or_else(|| Error::Config("semantic search needs an embedder".into()))?;
    Ok(Some(
        semantic_ids(conn, embedder, q, text)?
            .into_iter()
            .map(|(id, d)| (id, Some(d)))
            .collect(),
    ))
}

/// Semantic phrase search: KNN over the vec0 table, nearest first.
fn semantic_ids(
    conn: &Connection,
    embedder: &dyn Embedder,
    q: &Query,
    text: &str,
) -> Result<Vec<(i64, f32)>> {
    let query_vector = embedder.embed_query(text)?;
    let hint = index::vec::Knn {
        k: index::vec::count(conn)?.max(1) as usize,
        max_distance: q.max_distance,
    };
    index::vec::knn(conn, &query_vector, &hint)
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
            let Some(mut memory) = store::get_by_id(conn, nid)? else {
                continue;
            };
            memory.body = snippet(&memory.body, SNIPPET_CHARS);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_keeps_short_and_truncates_long() {
        assert_eq!(snippet("short", 10), "short");
        assert_eq!(snippet("", 0), "");
        // Truncation is on a char boundary, not a byte index.
        assert_eq!(snippet(&"á".repeat(5), 3), "ááá…");
    }

    #[test]
    fn a_blank_phrase_is_no_constraint() {
        let q = Query {
            text: Some("   ".to_string()),
            k: 10,
            max_distance: None,
            expand: 0,
        };
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // No phrase ⇒ no candidates, and no embedder required.
        assert!(phrase_ids(&conn, None, &q).unwrap().is_none());
    }
}
