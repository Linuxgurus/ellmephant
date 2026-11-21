pub mod lexical;

pub use lexical::MatchMode;

pub const SNIPPET_CHARS: usize = 280;

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
use crate::store::audience;
use crate::store::{self, Memory};

type Candidate = (i64, Option<f32>);

#[derive(Debug, Clone)]
pub struct Query {
    pub text: Option<String>,
    pub pattern: Option<String>,
    pub mode: MatchMode,
    pub k: usize,
    pub max_distance: Option<f32>,
    pub semantic_only: bool,
    pub expand: u8,
    pub tags: Vec<String>,
    pub min_chars: Option<usize>,
    pub max_chars: Option<usize>,
    pub created_after: Option<String>,
    pub created_before: Option<String>,
    pub updated_after: Option<String>,
    pub updated_before: Option<String>,
    pub key: Option<String>,
    pub key_prefix: bool,
    pub orphans: bool,
    pub untagged: bool,
    pub title: Option<String>,
    pub status: Option<String>,
}

impl Query {
    fn has_structured(&self) -> bool {
        !self.tags.is_empty()
            || self.min_chars.is_some()
            || self.max_chars.is_some()
            || self.created_after.is_some()
            || self.created_before.is_some()
            || self.updated_after.is_some()
            || self.updated_before.is_some()
    }

    fn has_filters(&self) -> bool {
        self.has_structured() || self.orphans || self.untagged
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    Semantic,
    Lexical,
    Both,
    Listing,
}

impl Via {
    pub fn name(self) -> &'static str {
        match self {
            Via::Semantic => "semantic",
            Via::Lexical => "lexical",
            Via::Both => "both",
            Via::Listing => "listing",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    #[serde(skip)]
    pub id: i64,
    pub key: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default)]
    pub chars: usize,
    pub via: Via,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Neighbor {
    pub hop: u8,
    pub via: String,
    pub memory: Memory,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bundle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<Vec<Hit>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lexical: Option<Vec<Hit>>,
    pub hits: Vec<Hit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub neighbors: Vec<Neighbor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<SearchMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMeta {
    pub max_distance: Option<f32>,
    pub semantic_candidates: usize,
    pub lexical_candidates: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predicates: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Key,
    Via,
    Title,
    Tags,
    Chars,
    Distance,
    Score,
}

impl Field {
    pub const ALL: [Field; 7] = [
        Field::Key,
        Field::Via,
        Field::Title,
        Field::Tags,
        Field::Chars,
        Field::Distance,
        Field::Score,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Field::Key => "key",
            Field::Via => "via",
            Field::Title => "title",
            Field::Tags => "tags",
            Field::Chars => "chars",
            Field::Distance => "distance",
            Field::Score => "score",
        }
    }

    pub fn parse(s: &str) -> Option<Field> {
        Field::ALL.into_iter().find(|f| f.name() == s)
    }
}

#[derive(Debug, Clone)]
pub struct Fields(Vec<Field>);

impl Fields {
    pub fn key_only() -> Self {
        Self::selection(Vec::new())
    }

    pub fn selection(wanted: impl IntoIterator<Item = Field>) -> Self {
        let wanted: Vec<Field> = wanted.into_iter().collect();
        Self(
            Field::ALL
                .into_iter()
                .filter(|f| matches!(f, Field::Key | Field::Via) || wanted.contains(f))
                .collect(),
        )
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.0.iter().map(|f| f.name())
    }

    pub fn row(&self, h: &Hit) -> serde_json::Value {
        serde_json::Value::Array(self.0.iter().map(|f| self.value(h, *f)).collect())
    }

    fn value(&self, h: &Hit, f: Field) -> serde_json::Value {
        match f {
            Field::Key => serde_json::json!(h.key),
            Field::Via => serde_json::json!(h.via.name()),
            Field::Title => serde_json::json!(h.title),
            Field::Tags => serde_json::json!(h.tags),
            Field::Chars => serde_json::json!(h.chars),
            Field::Distance => serde_json::json!(h.distance.map(crate::wire::wire_distance)),
            Field::Score => serde_json::json!(h.score),
        }
    }
}

pub fn run(
    conn: &Connection,
    embedder: Option<&dyn Embedder>,
    reader: &audience::Reader,
    q: &Query,
) -> Result<Bundle> {
    let mut phrase = phrase_ids(conn, embedder, q)?;
    let ranked = phrase.is_some();

    let mut lexical_leg = phrase_ids_lexical(conn, q)?;
    let mut lex_scores: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
    let mut lexical: Option<Vec<Candidate>> = lexical_leg.take().map(|(ids, scores)| {
        lex_scores = scores;
        ids
    });
    let visible = reader.visible_ids(conn)?;
    let hidden = |id: &i64| visible.as_ref().is_some_and(|v| !v.contains(id));
    if let Some(p) = phrase.as_mut() {
        p.retain(|(id, _)| !hidden(id));
    }
    if let Some(l) = lexical.as_mut() {
        l.retain(|(id, _)| !hidden(id));
        lex_scores.retain(|id, _| !hidden(id));
    }
    let semantic_ids: HashSet<i64> = phrase.iter().flatten().map(|(id, _)| *id).collect();
    let lexical_ids: HashSet<i64> = lexical.iter().flatten().map(|(id, _)| *id).collect();

    let structured = filter_set(conn, q)?;
    let pattern_set = lexical_set(conn, q)?;
    let keep = |id: &i64| {
        structured.as_ref().is_none_or(|s| s.contains(id))
            && pattern_set.as_ref().is_none_or(|s| s.contains(id))
    };
    if let Some(p) = phrase.as_mut() {
        p.retain(|(id, _)| keep(id));
    }
    if let Some(l) = lexical.as_mut() {
        l.retain(|(id, _)| keep(id));
    }
    lex_scores.retain(|id, _| keep(id));

    let mut ordered: Vec<(i64, Option<f32>)> = match (&phrase, &lexical) {
        (Some(p), _) if q.semantic_only => p.clone(),
        (Some(p), Some(l)) => fuse(p, l, q.k.max(1)),
        (Some(p), None) => p.clone(),
        (None, Some(l)) => l.clone(),
        (None, None) => all_ranked(conn)?,
    };
    ordered.retain(|(id, _)| !hidden(id));
    ordered.retain(|(id, _)| keep(id));
    let mut predicates: Vec<String> = Vec::new();
    let mut meta = SearchMeta {
        max_distance: q.max_distance,
        semantic_candidates: semantic_ids.len(),
        lexical_candidates: lexical_ids.len(),
        predicates: predicates.clone(),
    };
    if q.status.is_some() {
        return Err(crate::error::Error::Invalid(
            "`status` filters PROJECTS: this search is over memories. Use `project_list` with \
             `status`, or drop the parameter — it is not applied here."
                .into(),
        ));
    }
    if let Some(set) = predicate_set(conn, q, &mut predicates)? {
        ordered.retain(|(id, _)| set.contains(id));
    }
    meta.predicates = predicates.clone();

    if q.semantic_only {
        ordered.retain(|(id, _)| semantic_ids.contains(id));
        ordered.truncate(q.k.max(1));
    } else if ranked || lexical.is_some() || q.has_filters() {
        ordered.truncate(q.k.max(1));
    }

    let mut hits = Vec::with_capacity(ordered.len());
    for (id, distance) in ordered {
        let Some(mem) = store::get_by_id(conn, id)? else {
            continue;
        };
        if !reader.sees(&mem.audience) {
            continue;
        }
        let chars = embedded_len(&mem);
        let via = match (semantic_ids.contains(&id), lexical_ids.contains(&id)) {
            (true, true) => Via::Both,
            (true, false) => Via::Semantic,
            (false, true) => Via::Lexical,
            (false, false) => Via::Listing,
        };
        hits.push(Hit {
            id,
            via,
            key: mem.key,
            title: mem.title,
            tags: mem.tags,
            chars,
            distance,
            score: lex_scores.get(&id).copied(),
        });
    }

    let leg = |ids: &[Candidate], k: usize| -> Result<Vec<Hit>> {
        let mut out = Vec::new();
        for (id, distance) in ids.iter().take(k) {
            let Some(mem) = store::get_by_id(conn, *id)? else {
                continue;
            };
            if !reader.sees(&mem.audience) {
                continue;
            }
            let via = match (semantic_ids.contains(id), lexical_ids.contains(id)) {
                (true, true) => Via::Both,
                (true, false) => Via::Semantic,
                (false, true) => Via::Lexical,
                (false, false) => Via::Listing,
            };
            let chars = embedded_len(&mem);
            out.push(Hit {
                id: *id,
                via,
                key: mem.key,
                title: mem.title,
                tags: mem.tags,
                chars,
                distance: *distance,
                score: lex_scores.get(id).copied(),
            });
        }
        Ok(out)
    };
    let cap = q.k.max(1);
    let semantic_rows = match &phrase {
        Some(p) => Some(leg(p, cap)?),
        None => None,
    };
    let lexical_rows = match &lexical {
        Some(l) => Some(leg(l, cap)?),
        None => None,
    };

    let neighbors = if q.expand > 0 {
        expand(conn, &hits, q.expand, &visible)?
    } else {
        Vec::new()
    };
    Ok(Bundle {
        semantic: semantic_rows,
        lexical: lexical_rows,
        hits,
        neighbors,
        search: Some(meta),
    })
}

fn predicate_set(
    conn: &Connection,
    q: &Query,
    applied: &mut Vec<String>,
) -> Result<Option<HashSet<i64>>> {
    let mut sets: Vec<HashSet<i64>> = Vec::new();
    if let Some(key) = q.key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if q.key_prefix {
            let mut st = conn.prepare("SELECT id FROM memory WHERE key LIKE ?1 || '%'")?;
            let ids = st
                .query_map(rusqlite::params![key], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<HashSet<i64>>>()?;
            applied.push("key_prefix".into());
            sets.push(ids);
        } else {
            let mut st = conn.prepare("SELECT id FROM memory WHERE key = ?1")?;
            let ids = st
                .query_map(rusqlite::params![key], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<HashSet<i64>>>()?;
            applied.push("key".into());
            sets.push(ids);
        }
    }
    if let Some(title) = q.title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let term = title.replace('%', "\\%").replace('_', "\\_");
        let mut st =
            conn.prepare("SELECT id FROM memory WHERE title LIKE '%' || ?1 || '%' ESCAPE '\\'")?;
        let ids = st
            .query_map(rusqlite::params![term], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<HashSet<i64>>>()?;
        applied.push("title".into());
        sets.push(ids);
    }
    if q.orphans {
        let mut st = conn.prepare(
            "SELECT m.id FROM memory m
              WHERE m.id NOT IN (SELECT from_id FROM memory_link)
                AND m.key NOT IN (SELECT to_key FROM memory_link WHERE to_type = 'memory')",
        )?;
        let ids = st
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<HashSet<i64>>>()?;
        applied.push("orphans".into());
        sets.push(ids);
    }
    if q.untagged {
        let mut st = conn.prepare("SELECT id FROM memory WHERE tags = ''")?;
        let ids = st
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<HashSet<i64>>>()?;
        applied.push("untagged".into());
        sets.push(ids);
    }
    if sets.is_empty() {
        return Ok(None);
    }
    let mut out = sets.remove(0);
    for s in sets {
        out.retain(|id| s.contains(id));
    }
    Ok(Some(out))
}

#[allow(clippy::type_complexity)]
fn phrase_ids_lexical(
    conn: &Connection,
    q: &Query,
) -> Result<Option<(Vec<Candidate>, std::collections::HashMap<i64, f64>)>> {
    let Some(text) = q.text.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let want = (q.k.max(1) * 4).max(50);
    let hits = index::fts::search(conn, text, want)?;
    if hits.is_empty() {
        return Ok(Some((Vec::new(), std::collections::HashMap::new())));
    }
    let scores: std::collections::HashMap<i64, f64> = hits.iter().map(|h| (h.id, h.bm25)).collect();
    Ok(Some((
        hits.into_iter().map(|h| (h.id, None)).collect(),
        scores,
    )))
}

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

fn lexical_set(conn: &Connection, q: &Query) -> Result<Option<HashSet<i64>>> {
    let Some(pattern) = q
        .pattern
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
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

fn passes(mem: &Memory, q: &Query) -> bool {
    if !q.tags.iter().all(|want| {
        crate::store::tags::normalize(want).is_some_and(|want| mem.tags.iter().any(|t| t == &want))
    }) {
        return false;
    }
    let chars = embedded_len(mem);
    if q.min_chars.is_some_and(|min| chars < min) || q.max_chars.is_some_and(|max| chars > max) {
        return false;
    }
    if !within(&q.created_after, &q.created_before, &mem.created)
        || !within(&q.updated_after, &q.updated_before, &mem.updated)
    {
        return false;
    }
    true
}

fn within(after: &Option<String>, before: &Option<String>, value: &str) -> bool {
    after.as_deref().is_none_or(|lo| value >= lo) && before.as_deref().is_none_or(|hi| value <= hi)
}

fn embedded_len(mem: &Memory) -> usize {
    projection::embed_document(&mem.key, &mem.title, &mem.body)
        .chars()
        .count()
}

fn all_ranked(conn: &Connection) -> Result<Vec<(i64, Option<f32>)>> {
    let mut stmt = conn.prepare("SELECT id FROM memory ORDER BY key")?;
    let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push((row?, None));
    }
    Ok(out)
}

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

fn semantic_ids(
    conn: &Connection,
    embedder: &dyn Embedder,
    q: &Query,
    text: &str,
) -> Result<Vec<(i64, f32)>> {
    let query_vector = embedder.embed_query(text)?;
    let indexed = index::vec::count(conn)?;
    if indexed == 0 {
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

pub fn expand(
    conn: &Connection,
    seeds: &[Hit],
    hops: u8,
    visible: &Option<HashSet<i64>>,
) -> Result<Vec<Neighbor>> {
    let mut visited: HashSet<i64> = seeds.iter().map(|h| h.id).collect();
    let mut frontier: Vec<(i64, String)> = seeds.iter().map(|h| (h.id, h.key.clone())).collect();
    let mut out = Vec::new();

    for hop in 1..=hops.max(1) {
        frontier = expand_frontier(conn, &frontier, hop, &mut visited, &mut out, visible)?;
        if frontier.is_empty() {
            break;
        }
    }
    Ok(out)
}

fn expand_frontier(
    conn: &Connection,
    frontier: &[(i64, String)],
    hop: u8,
    visited: &mut HashSet<i64>,
    out: &mut Vec<Neighbor>,
    visible: &Option<HashSet<i64>>,
) -> Result<Vec<(i64, String)>> {
    let mut next = Vec::new();
    for (id, key) in frontier {
        for (nid, nkey) in adjacent(conn, *id, key)? {
            if visible.as_ref().is_some_and(|v| !v.contains(&nid)) {
                continue;
            }
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
            semantic_only: false,
            expand: 0,
            tags: Vec::new(),
            min_chars: None,
            max_chars: None,
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
            key: None,
            key_prefix: false,
            orphans: false,
            untagged: false,
            title: None,
            status: None,
        };
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        assert!(phrase_ids(&conn, None, &q).unwrap().is_none());
    }
}

#[cfg(test)]
mod fusion_tests {
    use super::*;

    #[test]
    fn rrf_rewards_a_row_both_legs_found() {
        let vector: Vec<Candidate> = vec![(1, Some(0.10)), (2, Some(0.20))];
        let lexical: Vec<Candidate> = vec![(2, None), (3, None)];
        let out = fuse(&vector, &lexical, 10);
        assert_eq!(out[0].0, 2, "the row both legs found wins: {out:?}");
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
