//! Wiki links: `[[key]]` in a body is parsed into `memory_link` edges (a derived
//! index) and stripped from the embed projection (SPEC §8). Rename and the dangling
//! audit live here too — both are link-graph maintenance (PLAN §3, P9).

use std::sync::LazyLock;

use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::embed::{Embedder, projection};
use crate::error::{Error, Result};
use crate::index;
use crate::store::{slug, tags};

/// Capture a `[[key]]` or `[[prj:key]]` target. Key charset `[a-z0-9-]+`; the `prj:` marker is the
/// only thing allowed before it.
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[((?:prj:)?[a-z0-9-]+)\]\]").expect("link regex is valid"));

/// Walk `s`, splitting it into alternating `(is_code, segment)` runs at each backtick. The backtick
/// delimiters are not part of any segment, so joining the segments with `` ` `` rebuilds `s`
/// verbatim; that is how [`strip_links`] re-emits them.
///
/// A backtick toggles: text between an odd backtick and the next is a **code span**, where a
/// `[[token]]` is documentation, not a reference. This is the escape the parser used to lack — a
/// body about links writes `` the marker is `[[prj:x]]` ``, and that no longer becomes an edge.
fn spans(s: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut in_code = false;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        if ch == '`' {
            out.push((in_code, &s[start..i]));
            in_code = !in_code;
            start = i + 1;
        }
    }
    out.push((in_code, &s[start..]));
    out
}

/// Which table a key names.
///
/// A key is a plain name — two tables may each hold a `sessions` — so the type travels with the
/// *reference* and never with the name. The table is a row's type; this says which table an edge
/// points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A `memory` row.
    Memory,
    /// A `project` row.
    Project,
}

impl Kind {
    /// The stored spelling (`memory_link.to_type`).
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Memory => "memory",
            Kind::Project => "project",
        }
    }

    /// Read the stored spelling back. The column's `CHECK` constrains it to the two values, so
    /// anything that is not `project` is a memory.
    pub fn from_stored(raw: &str) -> Self {
        if raw == Kind::Project.as_str() {
            Kind::Project
        } else {
            Kind::Memory
        }
    }
}

/// The marker that names a **project** in a link token: `[[prj:chronos]]`.
///
/// Links live in memory bodies, so a bare `[[key]]` means a memory and only the other kind needs
/// saying. `:` cannot occur in a slug (`[a-z0-9-]`, see [`slug::slugify`]), so unlike the old
/// `prj-` prefix no name can ever squat on it.
pub const PROJECT_MARKER: &str = "prj:";

/// Parse a link token's target into its kind and bare key. The token has already matched the
/// [`LINK_RE`] charset, so what comes back is always a valid key.
pub fn parse_target(token: &str) -> (Kind, String) {
    match token.strip_prefix(PROJECT_MARKER) {
        Some(key) => (Kind::Project, key.to_string()),
        None => (Kind::Memory, token.to_string()),
    }
}

/// Extract link targets in first-seen order, de-duplicated. A token inside a backtick code span is
/// literal text, not a reference (see [`spans`]).
pub fn extract_links(body: &str) -> Vec<(Kind, String)> {
    let mut seen = Vec::new();
    for (is_code, segment) in spans(body) {
        if is_code {
            continue;
        }
        for cap in LINK_RE.captures_iter(segment) {
            let target = parse_target(&cap[1]);
            if !seen.contains(&target) {
                seen.push(target);
            }
        }
    }
    seen
}

/// Remove every `[[key]]` token from `s` (the embed projection needs this; SPEC §8). A token inside
/// a backtick code span is left alone — it is literal text the projection should keep, not a link to
/// strip (see [`spans`]).
pub fn strip_links(s: &str) -> String {
    spans(s)
        .into_iter()
        .map(|(is_code, segment)| {
            if is_code {
                segment.to_string()
            } else {
                LINK_RE.replace_all(segment, "").into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("`")
}

/// A `[[key]]` that resolves to no row — an allowed forward/renamed reference that the
/// audit surfaces (SPEC §8: dangling links are detected, not fatal).
#[derive(Debug, Clone, Serialize)]
pub struct Dangling {
    /// The missing target key.
    pub to_key: String,
    /// Which table it was expected in — a missing memory and a missing project are different rot.
    pub to_type: Kind,
    /// Keys of the rows whose bodies point at it, sorted.
    pub from_keys: Vec<String>,
}

/// Group ordered `(to_key, to_type, from_key)` rows into [`Dangling`]s. Shared by the `[[…]]` audit
/// and the project audit, which both count rot the same way: one entry per unresolved target. The
/// GROUP is the pair — the same bare key can be missing as both a memory and a project.
pub fn group_dangling(rows: Vec<(String, Kind, String)>) -> Result<Vec<Dangling>> {
    let mut out: Vec<Dangling> = Vec::new();
    for (to_key, to_type, from_key) in rows {
        match out.last_mut() {
            Some(group) if group.to_key == to_key && group.to_type == to_type => {
                group.from_keys.push(from_key);
            }
            _ => out.push(Dangling {
                to_key,
                to_type,
                from_keys: vec![from_key],
            }),
        }
    }
    Ok(out)
}

/// Every `[[key]]` target that resolves to nothing, grouped by target (SPEC §8 SQL), ordered by
/// target key.
///
/// **A target resolves against the table its kind names** — a memory may point at a project with
/// `[[prj:chronos]]`, so a project key is not rot, and `to_type` is what keeps that honest. The
/// same name in the other table does *not* satisfy the edge: a `prj:sessions` link is not fixed by
/// a memory called `sessions`. A link to a project is still a `see` edge and not membership:
/// `contains` means "born here", a link only points.
pub fn dangling(conn: &Connection) -> Result<Vec<Dangling>> {
    let mut stmt = conn.prepare(
        "SELECT l.to_key, l.to_type, m.key FROM memory_link l
           JOIN memory m ON m.id = l.from_id
          WHERE (l.to_type = 'memory'  AND l.to_key NOT IN (SELECT key FROM memory))
             OR (l.to_type = 'project' AND l.to_key NOT IN (SELECT key FROM project))
          ORDER BY l.to_key, m.key",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Kind::from_stored(&r.get::<_, String>(1)?),
            r.get::<_, String>(2)?,
        ))
    })?;
    group_dangling(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Rebuild the `memory_link` graph for every memory from its **current** body.
///
/// `memory_link` is documented as *derived*, but edges are computed at **write** time ([`write_links`],
/// which this reuses) — so a parser change, or a body written by an older version, leaves a stored
/// graph the rows no longer imply. Two stores with identical rows could otherwise hold different
/// graphs, and a body that documented the link syntax under an old parser keeps an edge it should
/// not ([`spans`]). This recomputes every row's outgoing edges, so the graph really is *rebuildable*
/// from its source of truth; the `memory_reembed` tool runs it alongside the vector rebuild.
///
/// Returns the number of rows visited.
pub fn reparse(conn: &Connection) -> Result<usize> {
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, key, body FROM memory ORDER BY id")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let tx = conn.unchecked_transaction()?;
    for (id, key, body) in &rows {
        // One home for the edge write: `write_links` deletes the row's edges and re-inserts those
        // its body implies, so the reparse cannot drift from the write path.
        crate::store::write_links(&tx, *id, key, body)?;
    }
    tx.commit()?;
    Ok(rows.len())
}

/// What a [`rename`] changed.
#[derive(Debug, Clone)]
pub struct RenameOutcome {
    /// The resolved new key.
    pub key: String,
    /// Bodies whose `[[old]]` token was rewritten.
    pub bodies: usize,
    /// `memory_link` edges repointed from `old` to `new`.
    pub edges: usize,
}

/// Rename a key in one transaction (SPEC §15 #3, PLAN §2 #3): move `memory.key`, re-embed the row,
/// rewrite every `[[old]]` token in every body, and repoint the incoming `memory_link` edges.
///
/// Keys are near-immutable, so this is deliberate rather than routine. **`key` IS in the embed
/// projection**, so a rename changes the renamed row's document — its stored vector and `doc_hash`
/// would otherwise keep describing the old key until something else touched the row. So the row is
/// re-embedded here, the way [`crate::store::add`] does.
///
/// The rows whose bodies had `[[old]]` rewritten do **not** need it: `[[…]]` is stripped from the
/// projection (SPEC §8), so their document — and therefore their vector — is unchanged by the token
/// swap. (Their `updated` moves; that is the whole of it.)
///
/// Refuses to clobber an existing key; a same-key rename is a no-op.
pub fn rename(
    conn: &Connection,
    embedder: &dyn Embedder,
    old: &str,
    new: &str,
) -> Result<RenameOutcome> {
    let old = slug::slugify(old);
    let new = slug::slugify(new);
    if old == new {
        return Ok(RenameOutcome {
            key: new,
            bodies: 0,
            edges: 0,
        });
    }

    // Read the row before moving it: the new document needs its title and tags, and the vector
    // needs its rowid. `source` is not read — it is not part of the embed projection.
    let row: Option<(i64, String, String, String)> = conn
        .query_row(
            "SELECT id, title, body, tags FROM memory WHERE key = ?1",
            params![old],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((id, title, body, tags_raw)) = row else {
        return Err(Error::NotFound(old));
    };
    let taken: Option<i64> = conn
        .query_row("SELECT id FROM memory WHERE key = ?1", params![new], |r| {
            r.get(0)
        })
        .optional()?;
    if taken.is_some() {
        return Err(Error::Config(format!("key {new:?} already exists")));
    }

    // The document under the NEW key, hashed and embedded before the write lock — inference is
    // slow and must not hold the lock (SPEC §13). The vec table is created outside the tx too.
    let doc_text = projection::embed_document(&new, &tags::decode(&tags_raw), &title, &body);
    let doc_hash = crate::store::row_hash(embedder, &doc_text);
    index::vec::ensure(conn, embedder.dim())?;
    let vector = embedder.embed_one(&doc_text)?;

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE memory SET key = ?2, doc_hash = ?3, updated = datetime('now') WHERE id = ?1",
        params![id, new, doc_hash],
    )?;
    // Keys are `[a-z0-9-]+`, so the literal token can only match the exact link; the LIKE
    // prefilter keeps the scan bounded (and its `%` are not user-controlled).
    let bodies = tx.execute(
        "UPDATE memory SET body = replace(body, ?1, ?2), updated = datetime('now')
          WHERE body LIKE ?3",
        params![
            format!("[[{old}]]"),
            format!("[[{new}]]"),
            format!("%[[{old}]]%")
        ],
    )?;
    // OR REPLACE clears a would-be PK collision when a source already links to `new`. Scoped to
    // `memory`: a bare key can also name a *project*, and renaming a memory must not repoint edges
    // that were aimed at it.
    let edges = tx.execute(
        "UPDATE OR REPLACE memory_link SET to_key = ?2
          WHERE to_key = ?1 AND to_type = 'memory'",
        params![old, new],
    )?;
    index::vec::upsert(&tx, id, &vector)?;
    tx.commit()?;

    Ok(RenameOutcome {
        key: new,
        bodies,
        edges,
    })
}

/// One incoming `[[key]]` edge, as returned by [`backlinks`].
#[derive(Debug, Serialize)]
pub struct Backlink {
    /// The linking memory's key.
    pub key: String,
    /// Its salient line.
    pub title: String,
}

/// Incoming `[[…]]` edges for `key`, ordered by source key (the MCP `memory_backlinks`).
///
/// Memory-kind edges only: a `[[prj:sessions]]` link is aimed at the project, so it is not a
/// backlink of a memory called `sessions`.
pub fn backlinks(conn: &Connection, key: &str) -> Result<Vec<Backlink>> {
    let mut stmt = conn.prepare(
        "SELECT m.key, m.title FROM memory_link l JOIN memory m ON m.id = l.from_id
          WHERE l.to_key = ?1 AND l.to_type = 'memory' ORDER BY m.key",
    )?;
    let rows = stmt.query_map(params![key], |r| {
        Ok(Backlink {
            key: r.get(0)?,
            title: r.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Serialize a [`Backlink`] slice for the MCP wire: a top-level `cols` header names the fields
/// and `backlinks` are positional rows, so field names appear once rather than on every row
/// (mirrors [`crate::search::to_wire_json`]). The keyed serde form stays for scripts. Each row
/// sits on its own line (see [`crate::wire::rows_array`]) so a raw result is scannable.
pub fn backlinks_to_wire_json(rows: &[Backlink]) -> Result<String> {
    let backlinks: Vec<serde_json::Value> = rows
        .iter()
        .map(|b| serde_json::json!([b.key, b.title]))
        .collect();
    Ok(format!(
        "{{\"cols\":[\"key\",\"title\"],\"backlinks\":{}}}",
        crate::wire::rows_array(&backlinks)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_dedupes_in_order() {
        let body = "see [[b]] then [[a]] and [[b]] again";
        assert_eq!(
            extract_links(body),
            vec![
                (Kind::Memory, "b".to_string()),
                (Kind::Memory, "a".to_string())
            ]
        );
    }

    #[test]
    fn the_project_marker_is_what_types_a_target() {
        // A bare token is a memory; `prj:` is what crosses to the other table — so one name can be
        // both, and both edges survive.
        assert_eq!(
            extract_links("[[sessions]] and [[prj:sessions]]"),
            vec![
                (Kind::Memory, "sessions".to_string()),
                (Kind::Project, "sessions".to_string())
            ]
        );
        // The legacy spelling is NOT a marker in prose: a body saying `[[prj-foo]]` meant a memory
        // called `prj-foo`. (`prj-` is only stripped from tool *arguments*, where the type is
        // already given by the argument's job.)
        assert_eq!(
            extract_links("[[prj-foo]]"),
            vec![(Kind::Memory, "prj-foo".to_string())]
        );
    }

    #[test]
    fn ignores_bad_keys() {
        assert!(
            extract_links("[[Upper Case]] [[has_underscore]] [[ok-2]]")
                .contains(&(Kind::Memory, "ok-2".to_string()))
        );
        assert_eq!(extract_links("[[has_underscore]]"), Vec::new());
        assert_eq!(
            extract_links("[[mem:x]]"),
            Vec::new(),
            "only `prj:` is a marker; `mem:` is not a token at all"
        );
    }

    #[test]
    fn a_backtick_code_span_escapes_a_token() {
        // Documenting the syntax must not itself be a write to the graph.
        assert_eq!(
            extract_links("write `[[key]]` to link, like [[real]]"),
            vec![(Kind::Memory, "real".to_string())],
            "only the prose token is a reference"
        );
        // An unclosed backtick makes the rest of the body code: nothing after it links.
        assert!(extract_links("oops `[[a]]").is_empty());
    }

    #[test]
    fn strip_links_keeps_code_spans_verbatim() {
        assert_eq!(strip_links("a `b` c"), "a `b` c", "backticks re-emitted");
        assert_eq!(
            strip_links("see [[x]] and `[[y]]`"),
            "see  and `[[y]]`",
            "the prose token is stripped, the code token kept"
        );
    }

    #[test]
    fn no_links_is_empty() {
        assert!(extract_links("plain text").is_empty());
    }

    #[test]
    fn backlinks_wire_is_positional_with_a_cols_header() {
        let rows = vec![
            Backlink {
                key: "a".into(),
                title: "A".into(),
            },
            Backlink {
                key: "b".into(),
                title: "B".into(),
            },
        ];
        let wire = backlinks_to_wire_json(&rows).unwrap();
        let v: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(v["cols"], serde_json::json!(["key", "title"]));
        assert_eq!(v["backlinks"][0], serde_json::json!(["a", "A"]));
        assert!(!wire.contains("\"key\":"), "no per-row keys: {wire}");
        // One row per line: header(+open), row, row, close.
        assert_eq!(wire.lines().count(), 4, "scannable rows: {wire}");
    }
}
