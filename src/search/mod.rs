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
    /// Coherence-verdict filter over `memory.coherent`. `None` = any.
    pub coherent: Option<Coherent>,
}

/// A predicate over `memory.coherent` (a nullable bool). `!X` reads "not X", so the six
/// values are exactly the six non-empty proper subsets of `{true, false, null}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coherent {
    /// `coherent = true` — judged one coherent topic.
    True,
    /// `coherent = false` — judged a grab-bag worth splitting.
    False,
    /// `coherent IS NULL` — never judged.
    Null,
    /// `NOT true` — false or null (everything not proven coherent; the review queue).
    NotTrue,
    /// `NOT false` — true or null.
    NotFalse,
    /// `NOT null` — judged at all (true or false).
    NotNull,
}

impl Coherent {
    /// Whether a row's `coherent` value satisfies this predicate.
    pub fn matches(self, value: Option<bool>) -> bool {
        match self {
            Coherent::True => value == Some(true),
            Coherent::False => value == Some(false),
            Coherent::Null => value.is_none(),
            Coherent::NotTrue => value != Some(true),
            Coherent::NotFalse => value != Some(false),
            Coherent::NotNull => value.is_some(),
        }
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
    /// Split-review verdict (§9): `Some(true)`/`Some(false)`/`None` (unjudged).
    #[serde(default)]
    pub coherent: Option<bool>,
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
    /// Split-review verdict (§9).
    Coherent,
    /// Embedded-document length (chars).
    Chars,
    /// Cosine distance (semantic ranking only).
    Distance,
}

impl Field {
    /// Every field, in canonical wire order.
    pub const ALL: [Field; 6] = [
        Field::Key,
        Field::Title,
        Field::Tags,
        Field::Coherent,
        Field::Chars,
        Field::Distance,
    ];

    /// The wire name (a `cols` header entry).
    pub fn name(self) -> &'static str {
        match self {
            Field::Key => "key",
            Field::Title => "title",
            Field::Tags => "tags",
            Field::Coherent => "coherent",
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
            Field::Coherent => serde_json::json!(h.coherent),
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

    let mut ordered: Vec<(i64, Option<f32>)> = match phrase {
        Some(p) => p,
        None => all_ranked(conn)?,
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
    if ranked {
        ordered.truncate(q.k.max(1)); // k caps the distance-ranked case only
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
            coherent: mem.coherent,
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

/// The ids passing every structured predicate (`tags`, size, `coherent`), or `None` when the
/// query carries none (no filtering). Size is the length of the row's embedded document.
fn filter_set(conn: &Connection, q: &Query) -> Result<Option<HashSet<i64>>> {
    if q.tags.is_empty() && q.min_chars.is_none() && q.max_chars.is_none() && q.coherent.is_none() {
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
    if !q
        .tags
        .iter()
        .all(|want| mem.tags.iter().any(|t| t.eq_ignore_ascii_case(want.trim())))
    {
        return false;
    }
    let chars = embedded_len(mem);
    if q.min_chars.is_some_and(|min| chars < min) || q.max_chars.is_some_and(|max| chars > max) {
        return false;
    }
    if q.coherent.is_some_and(|c| !c.matches(mem.coherent)) {
        return false;
    }
    true
}

/// Length (chars) of the document a row embeds: `key` + tags + `title` + `body`, links stripped.
fn embedded_len(mem: &Memory) -> usize {
    projection::embed_document(&mem.key, &mem.tags, &mem.title, &mem.body)
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
            coherent: None,
        };
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // No phrase ⇒ no candidates, and no embedder required.
        assert!(phrase_ids(&conn, None, &q).unwrap().is_none());
    }
}
