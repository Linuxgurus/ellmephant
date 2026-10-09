//! Wiki links: `[[key]]` in a body is parsed into `memory_link` edges (a derived
//! index) and stripped from the embed projection (SPEC §8). Rename and the dangling
//! audit live here too — both are link-graph maintenance (PLAN §3, P9).

use std::sync::LazyLock;

use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::store::slug;

/// Capture `[[key]]` targets, key charset `[a-z0-9-]+`.
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([a-z0-9-]+)\]\]").expect("link regex is valid"));

/// Extract link targets in first-seen order, de-duplicated.
pub fn extract_links(body: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for cap in LINK_RE.captures_iter(body) {
        let key = cap[1].to_string();
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    seen
}

/// Remove every `[[key]]` token from `s` (the embed projection needs this; SPEC §8).
pub fn strip_links(s: &str) -> String {
    LINK_RE.replace_all(s, "").to_string()
}

/// A `[[key]]` that resolves to no row — an allowed forward/renamed reference that the
/// audit surfaces (SPEC §8: dangling links are detected, not fatal).
#[derive(Debug, Clone, Serialize)]
pub struct Dangling {
    /// The missing target key.
    pub to_key: String,
    /// Keys of the rows whose bodies point at it, sorted.
    pub from_keys: Vec<String>,
}

/// Every `[[key]]` target with no matching `memory.key`, grouped by target (SPEC §8 SQL),
/// ordered by target key.
pub fn dangling(conn: &Connection) -> Result<Vec<Dangling>> {
    let mut stmt = conn.prepare(
        "SELECT l.to_key, m.key FROM memory_link l
           JOIN memory m ON m.id = l.from_id
          WHERE l.to_key NOT IN (SELECT key FROM memory)
          ORDER BY l.to_key, m.key",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;

    let mut out: Vec<Dangling> = Vec::new();
    for row in rows {
        let (to_key, from_key) = row?;
        match out.last_mut() {
            Some(group) if group.to_key == to_key => group.from_keys.push(from_key),
            _ => out.push(Dangling {
                to_key,
                from_keys: vec![from_key],
            }),
        }
    }
    Ok(out)
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

/// Rename a key in one transaction (SPEC §15 #3, PLAN §2 #3): move `memory.key`, rewrite
/// every `[[old]]` token in every body, and repoint the incoming `memory_link` edges.
///
/// Keys are near-immutable, so this is deliberate rather than routine. `[[…]]` is stripped
/// from the embed projection, so a rename never invalidates a stored vector. Refuses to
/// clobber an existing key; a same-key rename is a no-op.
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

    let tx = conn.unchecked_transaction()?;
    let id: Option<i64> = tx
        .query_row("SELECT id FROM memory WHERE key = ?1", params![old], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(id) = id else {
        return Err(Error::NotFound(old));
    };
    let taken: Option<i64> = tx
        .query_row("SELECT id FROM memory WHERE key = ?1", params![new], |r| {
            r.get(0)
        })
        .optional()?;
    if taken.is_some() {
        return Err(Error::Config(format!("key {new:?} already exists")));
    }

    tx.execute(
        "UPDATE memory SET key = ?2, updated = datetime('now') WHERE id = ?1",
        params![id, new],
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
    // OR REPLACE clears a would-be PK collision when a source already links to `new`.
    let edges = tx.execute(
        "UPDATE OR REPLACE memory_link SET to_key = ?2 WHERE to_key = ?1",
        params![old, new],
    )?;
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
pub fn backlinks(conn: &Connection, key: &str) -> Result<Vec<Backlink>> {
    let mut stmt = conn.prepare(
        "SELECT m.key, m.title FROM memory_link l JOIN memory m ON m.id = l.from_id
          WHERE l.to_key = ?1 ORDER BY m.key",
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
        assert_eq!(extract_links(body), vec!["b", "a"]);
    }

    #[test]
    fn ignores_bad_keys() {
        assert!(
            extract_links("[[Upper Case]] [[has_underscore]] [[ok-2]]")
                .contains(&"ok-2".to_string())
        );
        assert_eq!(extract_links("[[has_underscore]]"), Vec::<String>::new());
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
