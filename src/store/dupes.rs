//! Duplicate-candidate audit: pairs of memories whose document embeddings sit unusually close
//! (the finder for the §9 "merge redundant memories" rule).
//!
//! A *finder*, not a judge. The embedded document is `key` + tags + title + body, so
//! merely-related memories cluster here too — the ceiling is tight on purpose, and the caller
//! decides (fetch both, judge, then merge). This module never merges.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde::Serialize;

use crate::error::Result;
use crate::index::vec::{self, Knn};

/// Default cosine-distance ceiling. Tight on purpose: with key/tags folded into the document,
/// a loose ceiling returns same-topic neighbours, not duplicates.
pub const DEFAULT_MAX_DISTANCE: f32 = 0.05;

/// Nearest neighbours examined per memory (self excluded). A duplicate is the *nearest* row,
/// so a handful is plenty; a pair missed at one end is still seen from the other.
const NEIGHBOURS: usize = 3;

/// One candidate pair (keys and titles, no bodies).
#[derive(Debug, Clone, Serialize)]
pub struct DupePair {
    /// Lexicographically-smaller key, so a pair is emitted once.
    pub key_a: String,
    /// Title of `key_a`.
    pub title_a: String,
    /// The other key.
    pub key_b: String,
    /// Title of `key_b`.
    pub title_b: String,
    /// Cosine distance between the two documents.
    pub distance: f32,
}

/// Every pair of memories closer than `max_distance`, nearest first, capped at `limit`.
///
/// One KNN pass per memory; pairs are deduplicated (`a`,`b` == `b`,`a`) and no memory is its
/// own neighbour.
pub fn find(conn: &Connection, max_distance: f32, limit: usize) -> Result<Vec<DupePair>> {
    let rows = load_rows(conn)?;
    if rows.len() < 2 {
        return Ok(Vec::new());
    }
    // +1 so that after dropping the self-match we still have `NEIGHBOURS` others.
    let k = (NEIGHBOURS + 1).min(rows.len());

    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    let mut pairs: Vec<DupePair> = Vec::new();
    for &id in rows.keys() {
        let Some(vector) = vec::get(conn, id)? else {
            continue; // a row with no vector (should not happen) is simply skipped
        };
        for (other, distance) in vec::knn(
            conn,
            &vector,
            &Knn {
                k,
                max_distance: None,
            },
        )? {
            if other == id || distance > max_distance {
                continue;
            }
            let (lo, hi) = if id < other { (id, other) } else { (other, id) };
            if !seen.insert((lo, hi)) {
                continue;
            }
            let (Some((key_a, title_a)), Some((key_b, title_b))) = (rows.get(&lo), rows.get(&hi))
            else {
                continue;
            };
            pairs.push(DupePair {
                key_a: key_a.clone(),
                title_a: title_a.clone(),
                key_b: key_b.clone(),
                title_b: title_b.clone(),
                distance,
            });
        }
    }
    pairs.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    pairs.truncate(limit);
    Ok(pairs)
}

/// `id -> (key, title)` for every stored memory.
fn load_rows(conn: &Connection) -> Result<HashMap<i64, (String, String)>> {
    let mut stmt = conn.prepare("SELECT id, key, title FROM memory")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                (r.get::<_, String>(1)?, r.get::<_, String>(2)?),
            ))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

/// Positional wire for the pair list (mirrors the other list-shaped reads): a `cols` header and
/// bare-array rows. `distance` is rounded to 4 decimals.
pub fn to_wire_json(pairs: &[DupePair]) -> Result<String> {
    let rows: Vec<serde_json::Value> = pairs
        .iter()
        .map(|p| {
            serde_json::json!([
                p.key_a,
                p.title_a,
                p.key_b,
                p.title_b,
                ((p.distance as f64) * 10_000.0).round() / 10_000.0
            ])
        })
        .collect();
    Ok(format!(
        "{{\"cols\":[\"key_a\",\"title_a\",\"key_b\",\"title_b\",\"distance\"],\
         \"count\":{},\"pairs\":{}}}",
        pairs.len(),
        crate::wire::rows_array(&rows)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_is_a_positional_pair_table() {
        let pairs = vec![DupePair {
            key_a: "a".into(),
            title_a: "A".into(),
            key_b: "b".into(),
            title_b: "B".into(),
            distance: 0.012_345,
        }];
        let wire = to_wire_json(&pairs).unwrap();
        assert!(
            wire.starts_with(
                "{\"cols\":[\"key_a\",\"title_a\",\"key_b\",\"title_b\",\"distance\"]"
            )
        );
        assert!(wire.contains("\"count\":1"));
        assert!(wire.contains("[\"a\",\"A\",\"b\",\"B\",0.0123]"), "{wire}");
        assert!(to_wire_json(&[]).unwrap().contains("\"pairs\":[]"));
    }
}
