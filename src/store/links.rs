use std::sync::LazyLock;

use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::embed::projection;
use crate::error::{Error, Result};
use crate::index;
use crate::store::slug;

static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[((?:prj:)?[a-z0-9-]+)\]\]").expect("link regex is valid"));

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Memory,
    Project,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Memory => "memory",
            Kind::Project => "project",
        }
    }

    pub fn from_stored(raw: &str) -> Self {
        if raw == Kind::Project.as_str() {
            Kind::Project
        } else {
            Kind::Memory
        }
    }

    pub fn keyslug(self, key: &str) -> String {
        format!("{}:{}", self.as_str(), key)
    }

    pub fn from_keyslug(slug: &str) -> Option<(Kind, &str)> {
        let (kind, key) = slug.split_once(':')?;
        if key.is_empty() {
            return None;
        }
        let kind = match kind {
            "memory" => Kind::Memory,
            "project" => Kind::Project,
            _ => return None,
        };
        Some((kind, key))
    }

    pub fn is_embeddable(self) -> bool {
        match self {
            Kind::Memory => true,
            Kind::Project => false,
        }
    }
}

pub const PROJECT_MARKER: &str = "prj:";

pub fn parse_target(token: &str) -> (Kind, String) {
    match token.strip_prefix(PROJECT_MARKER) {
        Some(key) => (Kind::Project, key.to_string()),
        None => (Kind::Memory, token.to_string()),
    }
}

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

#[derive(Debug, Clone, Serialize)]
pub struct Dangling {
    pub to_key: String,
    pub to_type: Kind,
    pub from_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DanglingEdge {
    pub from_key: String,
    pub to_key: String,
    pub to_type: Kind,
}

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

pub fn dangling(conn: &Connection) -> Result<Vec<Dangling>> {
    group_dangling(dangling_rows(conn)?)
}

pub fn dangling_edges(conn: &Connection) -> Result<Vec<DanglingEdge>> {
    Ok(dangling_rows(conn)?
        .into_iter()
        .map(|(to_key, to_type, from_key)| DanglingEdge {
            from_key,
            to_key,
            to_type,
        })
        .collect())
}

pub fn dangling_targets(edges: &[DanglingEdge]) -> usize {
    let mut targets: Vec<(&str, Kind)> = edges
        .iter()
        .map(|e| (e.to_key.as_str(), e.to_type))
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets.len()
}

fn dangling_rows(conn: &Connection) -> Result<Vec<(String, Kind, String)>> {
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
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn reparse(conn: &Connection) -> Result<usize> {
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, key, body FROM memory ORDER BY id")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let tx = conn.unchecked_transaction()?;
    for (id, key, body) in &rows {
        crate::store::write_links(&tx, *id, key, body)?;
    }
    tx.commit()?;
    Ok(rows.len())
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeInto {
    pub from: String,
    pub into: String,
    pub bodies: usize,
    pub preloads: usize,
    pub integrated: Vec<String>,
}

fn missing_lines(from: &str, into: &str) -> Vec<String> {
    let into = squash(into);
    let mut out: Vec<String> = Vec::new();
    for line in from.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let line = squash(line);
        if !line.is_empty() && !into.contains(&line) && !out.contains(&line) {
            out.push(line);
        }
    }
    out
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn merge_into(
    conn: &Connection,
    from: &str,
    into: &str,
    acted_by: Option<&str>,
) -> Result<MergeInto> {
    let from = slug::slugify(from);
    let into = slug::slugify(into);
    crate::store::may_write(conn, &from, acted_by)?;
    crate::store::may_write(conn, &into, acted_by)?;
    if from == into {
        return Err(Error::Config(
            "a memory cannot be merged into itself".into(),
        ));
    }
    let rowid = |key: &str| -> Result<i64> {
        conn.query_row("SELECT id FROM memory WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| Error::NotFound(key.to_string()))
    };
    let from_id = rowid(&from)?;
    let into_id = rowid(&into)?;
    let from_body: String = conn.query_row(
        "SELECT body FROM memory WHERE id = ?1",
        params![from_id],
        |r| r.get(0),
    )?;
    let (into_title, into_body): (String, String) = conn.query_row(
        "SELECT title, body FROM memory WHERE id = ?1",
        params![into_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let integrated = missing_lines(&from_body, &into_body);

    let tx = conn.unchecked_transaction()?;
    if !integrated.is_empty() {
        let body = format!("{}\n{}", into_body.trim_end(), integrated.join("\n"));
        tx.execute(
            "UPDATE memory SET body = ?2, updated = datetime('now') WHERE id = ?1",
            params![into_id, body],
        )?;
        crate::store::write_links(&tx, into_id, &into, &body)?;
        let doc = projection::embed_document(&into, &into_title, &body);
        crate::store::encode::enqueue(
            &tx,
            &Kind::Memory.keyslug(&into),
            &doc,
            crate::store::encode::PRIORITY_LIVE,
        )?;
    }
    let bodies = tx.execute(
        "UPDATE memory SET body = replace(body, ?1, ?2), updated = datetime('now')
          WHERE body LIKE ?3",
        params![
            format!("[[{from}]]"),
            format!("[[{into}]]"),
            format!("%[[{from}]]%")
        ],
    )?;
    tx.execute(
        "UPDATE OR REPLACE memory_link SET to_key = ?2
          WHERE to_key = ?1 AND to_type = 'memory'",
        params![from, into],
    )?;
    tx.execute(
        "DELETE FROM memory_link
          WHERE to_type = 'memory'
            AND to_key = (SELECT key FROM memory WHERE id = memory_link.from_id)",
        [],
    )?;
    let preloads = crate::store::project::repoint_preload(&tx, &from, &into)?;
    tx.execute("DELETE FROM memory WHERE id = ?1", params![from_id])?;
    index::vec::delete(&tx, from_id)?;
    crate::store::encode::forget(&tx, &Kind::Memory.keyslug(&from))?;
    tx.commit()?;

    Ok(MergeInto {
        from,
        into,
        bodies,
        preloads,
        integrated,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct FoldOutcome {
    pub into: String,
    pub from: String,
    pub integrated: Vec<String>,
    pub changed: bool,
}

pub fn fold_into(
    conn: &Connection,
    from: &str,
    into: &str,
    acted_by: Option<&str>,
) -> Result<FoldOutcome> {
    let from = slug::slugify(from);
    let into = slug::slugify(into);
    if from == into {
        return Err(Error::Config(
            "a memory cannot be folded into itself".into(),
        ));
    }
    let Some(actor) = acted_by else {
        return Err(Error::Forbidden(
            "a fold must name the pen doing it: appending to a row is a write, and an append \
             nobody is named for is indistinguishable from a mistake"
                .into(),
        ));
    };

    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;

    let body_of = |key: &str| -> Result<(String, String)> {
        tx.query_row(
            "SELECT body, audience FROM memory WHERE key = ?1",
            params![key],
            |r| -> rusqlite::Result<(String, String)> { Ok((r.get(0)?, r.get(1)?)) },
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(key.to_string()))
    };
    let (into_id, into_title, into_body, into_audience) = tx
        .query_row(
            "SELECT id, title, body, audience FROM memory WHERE key = ?1",
            params![into],
            |r| -> rusqlite::Result<(i64, String, String, String)> {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            },
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(into.clone()))?;
    let (from_body, from_audience) = body_of(&from)?;

    if !crate::store::may_read_audience(&tx, actor, &from_audience)? {
        return Err(Error::Forbidden(format!(
            "cannot fold `{from}`: its audience does not include the pen doing it. Folding copies \
             prose, and copying is reading."
        )));
    }
    if !crate::store::may_read_audience(&tx, actor, &into_audience)? {
        return Err(Error::Forbidden(format!(
            "cannot fold into `{into}`: its audience does not include the pen doing it. A row you \
             cannot read is not a row you may append to."
        )));
    }
    crate::store::guard_protected_lease(&tx, &into, Some(actor))?;

    let integrated = missing_lines(&from_body, &into_body);
    if integrated.is_empty() {
        return Ok(FoldOutcome {
            into,
            from,
            integrated,
            changed: false,
        });
    }

    crate::store::version::snapshot(&tx, into_id, &into, Some(&format!("fold from {from}")))?;
    let body = format!("{}\n{}", into_body.trim_end(), integrated.join("\n"));
    tx.execute(
        "UPDATE memory SET body = ?2, edited_by = ?3, updated = datetime('now') WHERE id = ?1",
        params![into_id, body, acted_by],
    )?;
    crate::store::write_links(&tx, into_id, &into, &body)?;
    let doc = projection::embed_document(&into, &into_title, &body);
    crate::store::encode::enqueue(
        &tx,
        &Kind::Memory.keyslug(&into),
        &doc,
        crate::store::encode::PRIORITY_LIVE,
    )?;
    tx.execute(
        "INSERT INTO memory_fold (keeper_id, absorbed_key, actor) VALUES (?1, ?2, ?3)",
        params![into_id, from, acted_by],
    )?;
    tx.commit()?;
    Ok(FoldOutcome {
        into,
        from,
        integrated,
        changed: true,
    })
}

#[derive(Debug, Clone)]
pub struct RenameOutcome {
    pub key: String,
    pub bodies: usize,
    pub edges: usize,
}

pub fn rename(conn: &Connection, old: &str, new: &str) -> Result<RenameOutcome> {
    let old = slug::slugify(old);
    let new = slug::slugify(new);
    if old == new {
        return Ok(RenameOutcome {
            key: new,
            bodies: 0,
            edges: 0,
        });
    }

    let row: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, title, body FROM memory WHERE key = ?1",
            params![old],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((id, title, body)) = row else {
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

    let doc_text = projection::embed_document(&new, &title, &body);

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE memory SET key = ?2, updated = datetime('now') WHERE id = ?1",
        params![id, new],
    )?;
    let bodies = tx.execute(
        "UPDATE memory SET body = replace(body, ?1, ?2), updated = datetime('now')
          WHERE body LIKE ?3",
        params![
            format!("[[{old}]]"),
            format!("[[{new}]]"),
            format!("%[[{old}]]%")
        ],
    )?;
    let edges = tx.execute(
        "UPDATE OR REPLACE memory_link SET to_key = ?2
          WHERE to_key = ?1 AND to_type = 'memory'",
        params![old, new],
    )?;
    crate::store::encode::enqueue(
        &tx,
        &Kind::Memory.keyslug(&new),
        &doc_text,
        crate::store::encode::PRIORITY_LIVE,
    )?;
    tx.commit()?;

    Ok(RenameOutcome {
        key: new,
        bodies,
        edges,
    })
}

#[derive(Debug, Serialize)]
pub struct Backlink {
    pub key: String,
    pub title: String,
}

pub fn backlinks(conn: &Connection, key: &str) -> Result<Vec<Backlink>> {
    backlinks_seen(conn, key, &crate::store::audience::Reader::everything())
}

pub fn backlinks_seen(
    conn: &Connection,
    key: &str,
    reader: &crate::store::audience::Reader,
) -> Result<Vec<Backlink>> {
    let target: Option<String> = conn
        .query_row(
            "SELECT audience FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?;
    if target.is_some_and(|audience| !reader.sees(&audience)) {
        return Ok(Vec::new());
    }
    let (predicate, binds) = reader.sql();
    let mut stmt = conn.prepare(&format!(
        "SELECT m.key, m.title FROM memory_link l JOIN memory m ON m.id = l.from_id
          WHERE l.to_key = ?1 AND l.to_type = 'memory' AND ({predicate}) ORDER BY m.key"
    ))?;
    let mut all: Vec<rusqlite::types::Value> = vec![key.to_string().into()];
    all.extend(binds.into_iter().map(Into::into));
    let rows = stmt.query_map(rusqlite::params_from_iter(all), |r| {
        Ok(Backlink {
            key: r.get(0)?,
            title: r.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
        assert_eq!(
            extract_links("[[sessions]] and [[prj:sessions]]"),
            vec![
                (Kind::Memory, "sessions".to_string()),
                (Kind::Project, "sessions".to_string())
            ]
        );
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
        assert_eq!(
            extract_links("write `[[key]]` to link, like [[real]]"),
            vec![(Kind::Memory, "real".to_string())],
            "only the prose token is a reference"
        );
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
        let wire = crate::wire::links::backlinks_to_wire_json(&rows).unwrap();
        let v: serde_json::Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(v["cols"], serde_json::json!(["key", "title"]));
        assert_eq!(v["backlinks"][0], serde_json::json!(["a", "A"]));
        assert!(!wire.contains("\"key\":"), "no per-row keys: {wire}");
        assert_eq!(wire.lines().count(), 4, "scannable rows: {wire}");
    }
}
