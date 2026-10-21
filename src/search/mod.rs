//! Search: a semantic phrase plus an optional literal/regex leg, over the store (SPEC §10).
//!
//! Two legs feed one candidate set. The **semantic** leg embeds `text` as a query-side phrase and
//! takes the KNN over the stored document vectors (cosine distance). The **lexical** leg
//! ([`lexical`]) matches `pattern` as a literal substring or a regex against the *raw* `key`,
//! `title` and `body`. Both compose with the structured filters by **intersection** — every leg
//! and filter given must hold (the ledger's "filters intersect" law); there is no cross-leg fusion
//! ranking, so a lexical-only query is unranked (`key` order).
//!
//! Results are distance-ranked only when the phrase was semantic, then capped by `k` and a
//! `max_distance` ceiling. A hit returns its `key` by default and nothing else; the caller names
//! the extra [`Field`]s it wants (via [`Fields`]) — never a body; fetch that with
//! `memory_get`/`show`.

pub mod lexical;

pub use lexical::MatchMode;

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

use crate::embed::{Embedder, projection};
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
    /// Literal/regex pattern over `key`/`title`/`body`; `None`/blank means "no lexical leg".
    pub pattern: Option<String>,
    /// How `pattern` is read — a plain substring or a regex ([`MatchMode`]).
    pub mode: MatchMode,
    /// Result cap (semantic ranking only).
    pub k: usize,
    /// Optional cosine-distance ceiling.
    pub max_distance: Option<f32>,
    /// Link-neighbourhood hops to include (0 = off).
    pub expand: u8,
    /// Tag filter: **every** listed tag must be present (intersection). Empty = no constraint.
    pub tags: Vec<String>,
    /// Minimum embedded-document length in chars. `None` = no floor.
    pub min_chars: Option<usize>,
    /// Maximum embedded-document length in chars. `None` = no ceiling.
    pub max_chars: Option<usize>,
    /// Only rows whose `created` is at or after this ISO timestamp.
    pub created_after: Option<String>,
    /// Only rows whose `created` is at or before this ISO timestamp.
    pub created_before: Option<String>,
    /// Only rows whose `updated` is at or after this ISO timestamp.
    pub updated_after: Option<String>,
    /// Only rows whose `updated` is at or before this ISO timestamp.
    pub updated_before: Option<String>,
}

impl Query {
    /// Whether any structured predicate is set — if not, the filter leg has no work to do and
    /// [`filter_set`] returns `None` (no filtering) rather than scanning every row.
    fn has_structured(&self) -> bool {
        !self.tags.is_empty()
            || self.min_chars.is_some()
            || self.max_chars.is_some()
            || self.created_after.is_some()
            || self.created_before.is_some()
            || self.updated_after.is_some()
            || self.updated_before.is_some()
    }
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
    /// Length (chars) of the row's embedded document — the size the `min_chars`/`max_chars`
    /// filters key on. Lets a caller sort the fat tail itself (no server-side sort).
    #[serde(default)]
    pub chars: usize,
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

/// One selectable hit field on the MCP wire. `key` is always returned (it is the handle you
/// `memory_get` with); the rest are opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// Slug.
    Key,
    /// Salient line.
    Title,
    /// Attached tags.
    Tags,
    /// Embedded-document length (chars).
    Chars,
    /// Cosine distance (semantic ranking only).
    Distance,
}

impl Field {
    /// Every field, in canonical wire order.
    pub const ALL: [Field; 5] = [
        Field::Key,
        Field::Title,
        Field::Tags,
        Field::Chars,
        Field::Distance,
    ];

    /// The wire name (a `cols` header entry).
    pub fn name(self) -> &'static str {
        match self {
            Field::Key => "key",
            Field::Title => "title",
            Field::Tags => "tags",
            Field::Chars => "chars",
            Field::Distance => "distance",
        }
    }

    /// Parse a wire name; `None` when it is not a field.
    pub fn parse(s: &str) -> Option<Field> {
        Field::ALL.into_iter().find(|f| f.name() == s)
    }
}

/// The hit projection for the wire: which fields each row carries. `key` is always included — it
/// is the handle — so a selection really names the *extra* fields on top of it, emitted in
/// canonical order. The default ([`key_only`](Self::key_only)) keeps a targeted search to one
/// column (SPEC §10): a hit says *which* memory, and `memory_get` reads it.
#[derive(Debug, Clone)]
pub struct Fields(Vec<Field>);

impl Fields {
    /// Just `key` — the lean default.
    pub fn key_only() -> Self {
        Self(vec![Field::Key])
    }

    /// Canonical order; `key` is always present, `wanted` adds the rest (duplicates ignored).
    pub fn selection(wanted: impl IntoIterator<Item = Field>) -> Self {
        let wanted: Vec<Field> = wanted.into_iter().collect();
        Self(
            Field::ALL
                .into_iter()
                .filter(|f| *f == Field::Key || wanted.contains(f))
                .collect(),
        )
    }

    /// Column names, in canonical order.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.0.iter().map(|f| f.name())
    }

    /// One hit as a positional row matching [`names`](Self::names).
    pub fn row(&self, h: &Hit) -> serde_json::Value {
        serde_json::Value::Array(self.0.iter().map(|f| self.value(h, *f)).collect())
    }

    /// The JSON value of one field on one hit.
    fn value(&self, h: &Hit, f: Field) -> serde_json::Value {
        match f {
            Field::Key => serde_json::json!(h.key),
            Field::Title => serde_json::json!(h.title),
            Field::Tags => serde_json::json!(h.tags),
            Field::Chars => serde_json::json!(h.chars),
            Field::Distance => serde_json::json!(h.distance.map(wire_distance)),
        }
    }
}

/// Serialize a [`Bundle`] for the MCP wire: a top-level `cols` header names the hit fields and
/// `hits` are positional rows, so the payload stays self-describing without repeating field
/// names on every row. `fields` decides which columns ride along (see [`Fields`]; `key` is always
/// present). Link `neighbors`, when present, carry their own `cols`/`rows` table.
///
/// This is the compact LLM-facing shape (fewer tokens than keyed JSON); the keyed serde form
/// remains the stable machine face used by `--json` and `structuredContent`. Each row sits on
/// its own line (see [`crate::wire::rows_array`]) so a raw result is scannable, not one blob.
pub fn to_wire_json(bundle: &Bundle, fields: &Fields) -> Result<String> {
    let hits: Vec<serde_json::Value> = bundle.hits.iter().map(|h| fields.row(h)).collect();
    let header: String = fields
        .names()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(",");

    let mut out = format!("{{\"cols\":[{header}],\"hits\":");
    out.push_str(&crate::wire::rows_array(&hits));

    if !bundle.neighbors.is_empty() {
        let rows: Vec<serde_json::Value> = bundle
            .neighbors
            .iter()
            .map(|n| {
                serde_json::json!([
                    n.hop,
                    n.via,
                    n.memory.key,
                    n.memory.title,
                    n.memory.tags,
                    n.memory.body
                ])
            })
            .collect();
        out.push_str(
            ",\"neighbors\":{\"cols\":[\"hop\",\"via\",\"key\",\"title\",\"tags\",\"body\"],\"rows\":",
        );
        out.push_str(&crate::wire::rows_array(&rows));
        out.push('}');
    }
    out.push('}');
    Ok(out)
}

/// Round a distance to 4 decimals for the wire: enough to rank, and it avoids the long
/// `f32`→`f64` widening tail that `json!` would otherwise print (`0.41999998…`).
fn wire_distance(d: f32) -> f64 {
    ((d as f64) * 10_000.0).round() / 10_000.0
}

/// Run a query. `embedder` is required when a phrase is given, ignored otherwise.
pub fn run(conn: &Connection, embedder: Option<&dyn Embedder>, q: &Query) -> Result<Bundle> {
    let phrase = phrase_ids(conn, embedder, q)?;
    let ranked = phrase.is_some();

    // ★ TWO LEGS, FUSED BY RRF (soma/ellmephant#62). The semantic leg answers "means this"; the word
    // index answers "says this". Neither dominates: measured on 2026-10-21 the correct memory for
    // `runner has no cargo` ranked FIFTH at 0.5945 behind an unrelated 0.5795, while a paraphrase
    // shares no words at all and only the vector leg can find it.
    //
    // Reciprocal Rank Fusion combines RANKS, not scores, precisely because cosine distance and BM25
    // are different units in different directions (lower-better and negative-lower-better) — adding
    // them would need a calibration nobody has. `k=60` is the constant from the RRF literature: it
    // flattens the top of each list so one leg's runaway first place does not veto the other.
    let lexical = phrase_ids_lexical(conn, q)?;
    let mut ordered: Vec<(i64, Option<f32>)> = match (&phrase, &lexical) {
        (Some(p), Some(l)) => fuse(p, l, q.k.max(1)),
        (Some(p), None) => p.clone(),
        (None, Some(l)) => l.clone(),
        (None, None) => all_ranked(conn)?,
    };
    // Structured filters intersect the candidate set BEFORE the k cap, so a filtered result
    // is exact (the KNN already spans the whole store): filter, then truncate, never the reverse.
    if let Some(set) = filter_set(conn, q)? {
        ordered.retain(|(id, _)| set.contains(id));
    }
    // The lexical leg intersects the same way; a lexical-only query is unranked, so `k` does not
    // cap it (like the structured filters).
    if let Some(set) = lexical_set(conn, q)? {
        ordered.retain(|(id, _)| set.contains(id));
    }
    if ranked && lexical.is_none() {
        ordered.truncate(q.k.max(1)); // k caps a single-leg list; `fuse` already capped a fused one
    }

    let mut hits = Vec::with_capacity(ordered.len());
    for (id, distance) in ordered {
        let Some(mem) = store::get_by_id(conn, id)? else {
            continue; // the leg returned an id that vanished; skip, don't fail the query
        };
        let chars = embedded_len(&mem);
        hits.push(Hit {
            id,
            key: mem.key,
            title: mem.title,
            tags: mem.tags,
            chars,
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

/// The lexical leg: FTS5 over the same columns, ranked by BM25 (`soma/ellmephant#62`).
///
/// `None` when the query has no phrase or the phrase has nothing indexable. An FTS failure is an
/// error rather than an empty list: a leg that cannot run must not silently shrink a search that the
/// other leg would have answered — the caller asked for a phrase and deserves to know halving it was
/// not possible.
fn phrase_ids_lexical(conn: &Connection, q: &Query) -> Result<Option<Vec<Candidate>>> {
    let Some(text) = q.text.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    // Room for the lexical leg to contribute rows the vector leg missed, without dragging in the
    // whole store: four times the requested page, floored at 50.
    let want = (q.k.max(1) * 4).max(50);
    let hits = index::fts::search(conn, text, want)?;
    if hits.is_empty() {
        return Ok(None);
    }
    // `distance` stays None: BM25 is not a distance, and reporting a negative number in a field
    // documented as cosine distance is the unit confusion #59 exists to prevent.
    Ok(Some(hits.into_iter().map(|h| (h.id, None)).collect()))
}

/// Reciprocal Rank Fusion: `score = Σ 1/(60 + rank)` over the legs that found the row.
///
/// The vector leg's distance survives into the result (it is the only meaningful distance we have);
/// the lexical leg contributes only its rank. Ties break on the vector distance, so a row both legs
/// found sorts ahead of an equally-ranked lexical-only one.
fn fuse(vector: &[Candidate], lexical: &[(i64, Option<f32>)], k: usize) -> Vec<Candidate> {
    const K: f32 = 60.0;
    let mut score: std::collections::HashMap<i64, (f32, Option<f32>)> =
        std::collections::HashMap::new();
    for (rank, (id, distance)) in vector.iter().enumerate() {
        score.insert(*id, (1.0 / (K + rank as f32), *distance));
    }
    for (rank, (id, _)) in lexical.iter().enumerate() {
        let add = 1.0 / (K + rank as f32);
        score
            .entry(*id)
            .and_modify(|(s, _)| *s += add)
            .or_insert((add, None));
    }
    let mut out: Vec<(i64, f32, Option<f32>)> =
        score.into_iter().map(|(id, (s, d))| (id, s, d)).collect();
    // Fused score descending; then distance ascending (closer first); then id for determinism.
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.2.unwrap_or(f32::MAX)
                    .partial_cmp(&b.2.unwrap_or(f32::MAX))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.0.cmp(&b.0))
    });
    out.into_iter().take(k).map(|(id, _, d)| (id, d)).collect()
}

/// The ids passing every structured predicate (`tags`, size, time bounds), or `None`
/// when the query carries none (no filtering). Size is the length of the row's embedded document.
fn filter_set(conn: &Connection, q: &Query) -> Result<Option<HashSet<i64>>> {
    if !q.has_structured() {
        return Ok(None);
    }
    let mut set = HashSet::new();
    for mem in store::all(conn)? {
        if passes(&mem, q)
            && let Some(id) = mem.id
        {
            set.insert(id);
        }
    }
    Ok(Some(set))
}

/// The ids matching the lexical leg, or `None` when the query carries no pattern. A blank pattern
/// is no pattern (mirrors the blank-phrase rule).
fn lexical_set(conn: &Connection, q: &Query) -> Result<Option<HashSet<i64>>> {
    let Some(pattern) = q
        .pattern
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    // Compile once, then scan — a malformed regex fails here, before touching a row.
    let matcher = lexical::Matcher::new(pattern, q.mode)?;
    let mut set = HashSet::new();
    for mem in store::all(conn)? {
        if let Some(id) = mem.id
            && matcher.hits(&mem)
        {
            set.insert(id);
        }
    }
    Ok(Some(set))
}

/// Whether `mem` satisfies every structured predicate in `q` (tags AND, size range, coherence).
fn passes(mem: &Memory, q: &Query) -> bool {
    // A tag is normalised on the way IN (`tags::encode`: trim, lowercase, whitespace -> `-`), so a
    // filter must normalise the same way on the way OUT — otherwise a tag can be *written* that can
    // never be *queried*: `prj: seaweedfs` stores as `prj:-seaweedfs`, and the raw string never
    // matches it. (The project filter already did this; the memory one did not.)
    if !q.tags.iter().all(|want| {
        crate::store::tags::normalize(want).is_some_and(|want| mem.tags.iter().any(|t| t == &want))
    }) {
        return false;
    }
    let chars = embedded_len(mem);
    if q.min_chars.is_some_and(|min| chars < min) || q.max_chars.is_some_and(|max| chars > max) {
        return false;
    }
    // Time bounds compare the STORED ISO shape (`datetime('now')` writes `YYYY-MM-DD HH:MM:SS`)
    // lexicographically, which is why the shape is insisted on at write time — `>=` reads as "at or
    // after". A bound outside that shape is the caller's typo and simply matches nothing.
    if !within(&q.created_after, &q.created_before, &mem.created)
        || !within(&q.updated_after, &q.updated_before, &mem.updated)
    {
        return false;
    }
    true
}

/// Whether `value` (an ISO timestamp) lies inside an optional `[after, before]` window.
fn within(after: &Option<String>, before: &Option<String>, value: &str) -> bool {
    after.as_deref().is_none_or(|lo| value >= lo) && before.as_deref().is_none_or(|hi| value <= hi)
}

/// Length (chars) of the document a row embeds: `key` + `title` + `body`, links stripped.
fn embedded_len(mem: &Memory) -> usize {
    projection::embed_document(&mem.key, &mem.title, &mem.body)
        .chars()
        .count()
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
    let indexed = index::vec::count(conn)?;
    if indexed == 0 {
        // Nothing is indexed. An empty vector table WITH rows in `memory` is an unindexed store, not
        // an empty one: an empty hit list reads as "nothing matched", which is the one answer that is
        // false — so say what is wrong and how to fix it. No rows at all (a brand-new store) is
        // genuinely empty, and there is nothing to match.
        let rows: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
        if rows > 0 {
            return Err(Error::Unavailable(format!(
                "{rows} memory row(s) carry no vector — run `reembed`"
            )));
        }
        return Ok(Vec::new());
    }
    let hint = index::vec::Knn {
        k: indexed.max(1) as usize,
        max_distance: q.max_distance,
    };
    index::vec::knn(conn, &query_vector, &hint, Some("memory"))
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
///
/// Memory kind only, both ways: a `[[prj:x]]` edge points at a project, which is not a memory
/// neighbour — and because keys are bare, a project's edge would otherwise look like an edge to the
/// memory that happens to share its name.
fn adjacent(conn: &Connection, id: i64, key: &str) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.key FROM memory_link l JOIN memory m ON m.key = l.to_key
          WHERE l.from_id = ?1 AND l.to_type = 'memory'
         UNION
         SELECT m.id, m.key FROM memory_link l JOIN memory m ON m.id = l.from_id
          WHERE l.to_key = ?2 AND l.to_type = 'memory'",
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
            pattern: None,
            mode: MatchMode::Literal,
            k: 10,
            max_distance: None,
            expand: 0,
            tags: Vec::new(),
            min_chars: None,
            max_chars: None,
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
        };
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // No phrase ⇒ no candidates, and no embedder required.
        assert!(phrase_ids(&conn, None, &q).unwrap().is_none());
    }
}

#[cfg(test)]
mod fusion_tests {
    use super::*;

    /// ★ RRF pinned directly, because an end-to-end test with a fake embedder cannot prove it: the
    /// vector leg may happen to rank the right row anyway. Here the legs are given by hand.
    #[test]
    fn rrf_rewards_a_row_both_legs_found() {
        // Vector leg: A first, B second. Lexical leg: B first, C second.
        let vector: Vec<Candidate> = vec![(1, Some(0.10)), (2, Some(0.20))];
        let lexical: Vec<Candidate> = vec![(2, None), (3, None)];
        let out = fuse(&vector, &lexical, 10);
        // B is the only row both legs ranked, so it must lead: 1/(60+1) + 1/(60+0) beats any single.
        assert_eq!(out[0].0, 2, "the row both legs found wins: {out:?}");
        // A and C each appear once; A's vector distance survives, C's stays None.
        let a = out.iter().find(|(id, _)| *id == 1).expect("A present");
        assert_eq!(a.1, Some(0.10), "the vector distance is preserved");
        let c = out.iter().find(|(id, _)| *id == 3).expect("C present");
        assert_eq!(c.1, None, "a lexical-only row carries no distance");
    }

    #[test]
    fn rrf_keeps_a_row_only_one_leg_found() {
        let vector: Vec<Candidate> = vec![(1, Some(0.1))];
        let lexical: Vec<Candidate> = vec![(2, None)];
        let out = fuse(&vector, &lexical, 10);
        let ids: Vec<i64> = out.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1) && ids.contains(&2), "{ids:?}");
    }

    #[test]
    fn fuse_caps_at_k() {
        let vector: Vec<Candidate> = (0..20).map(|i| (i, Some(i as f32))).collect();
        let out = fuse(&vector, &[], 5);
        assert_eq!(out.len(), 5);
    }
}
