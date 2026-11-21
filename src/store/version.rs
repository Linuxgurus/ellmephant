use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;

pub const MEMORY: &str = "memory";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub n: i64,
    pub written_at: String,
    pub encoding: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
    pub bytes: i64,
}

pub(crate) fn snapshot(tx: &Connection, id: i64, key: &str, note: Option<&str>) -> Result<i64> {
    let (k, title, body, tags, source, created, updated, audience): (
        String,
        String,
        String,
        String,
        Option<String>,
        String,
        String,
        String,
    ) = tx.query_row(
        "SELECT key, title, body, tags, source, created, updated, audience FROM memory WHERE id = ?1",
        params![id],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        },
    )?;

    let content = serde_json::json!({
        "key": k,
        "title": title,
        "body": body,
        "tags": tags.split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
        "source": source,
        "created": created,
        "updated": updated,
        "audience": audience,
    })
    .to_string();

    let edited_by: Option<i64> = tx
        .query_row(
            "SELECT p.id FROM memory m JOIN pen p ON p.name = m.edited_by WHERE m.id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;

    let n: i64 = tx.query_row(
        "SELECT COALESCE(MAX(n), 0) + 1 FROM memory_version WHERE type = ?1 AND key = ?2",
        params![MEMORY, key],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO memory_version (type, key, n, encoding, note, content, edited_by)
         VALUES (?1, ?2, ?3, 'json', ?4, ?5, ?6)",
        params![MEMORY, key, n, note, content, edited_by],
    )?;
    if history_fts_ready(tx) {
        tx.execute(
            "INSERT INTO history_fts (body, key, n) VALUES (?1, ?2, ?3)",
            params![body, key, n],
        )?;
    }
    Ok(n)
}

pub fn history(conn: &Connection, key: &str) -> Result<Vec<Version>> {
    let mut stmt = conn.prepare(
        "SELECT n, written_at, encoding, note, length(content)
           FROM memory_version
          WHERE type = ?1 AND key = ?2
          ORDER BY n",
    )?;
    let rows = stmt.query_map(params![MEMORY, key], |r| {
        Ok(Version {
            n: r.get(0)?,
            written_at: r.get(1)?,
            encoding: r.get(2)?,
            note: r.get(3)?,
            bytes: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn history_fts_ready(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'history_fts'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

pub fn read(conn: &Connection, key: &str, n: i64) -> Result<Option<serde_json::Value>> {
    let found: Option<String> = conn
        .query_row(
            "SELECT content FROM memory_version WHERE type = ?1 AND key = ?2 AND n = ?3",
            params![MEMORY, key, n],
            |r| r.get(0),
        )
        .optional()?;
    match found {
        Some(c) => Ok(Some(serde_json::from_str(&c)?)),
        None => Ok(None),
    }
}

pub fn count(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT count(*), COALESCE(sum(length(content)), 0) FROM memory_version",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

pub fn read_latest(conn: &Connection, key: &str) -> Result<Option<serde_json::Value>> {
    let found: Option<String> = conn
        .query_row(
            "SELECT content FROM memory_version WHERE type = ?1 AND key = ?2 \
              ORDER BY n DESC LIMIT 1",
            params![MEMORY, key],
            |r| r.get(0),
        )
        .optional()?;
    match found {
        Some(text) => Ok(Some(serde_json::from_str(&text)?)),
        None => Ok(None),
    }
}

pub fn count_for(conn: &Connection, key: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM memory_version WHERE type = ?1 AND key = ?2",
        params![MEMORY, key],
        |r| r.get(0),
    )?)
}

pub fn erase(conn: &Connection, key: &str) -> Result<usize> {
    if history_fts_ready(conn) {
        conn.execute("DELETE FROM history_fts WHERE key = ?1", params![key])?;
    }
    Ok(conn.execute(
        "DELETE FROM memory_version WHERE type = ?1 AND key = ?2",
        params![MEMORY, key],
    )?)
}

pub fn orphan_keys(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT v.key FROM memory_version v
           LEFT JOIN memory m ON m.key = v.key
          WHERE v.type = ?1 AND m.id IS NULL
          GROUP BY v.key
          ORDER BY v.key",
    )?;
    let rows = stmt.query_map(params![MEMORY], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn erase_orphans(conn: &Connection) -> Result<(usize, usize)> {
    let keys = orphan_keys(conn)?;
    if keys.is_empty() {
        return Ok((0, 0));
    }
    let tx = conn.unchecked_transaction()?;
    let mut versions = 0usize;
    for k in &keys {
        versions += erase(&tx, k)?;
    }
    tx.commit()?;
    Ok((keys.len(), versions))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryIndex {
    pub key: String,
    pub versions: i64,
    pub last_written: String,
    pub orphan: bool,
}

pub fn index(conn: &Connection) -> Result<Vec<HistoryIndex>> {
    let mut stmt = conn.prepare(
        "SELECT v.key, count(*) AS versions, max(v.written_at) AS last_written,
                (m.id IS NULL) AS orphan
           FROM memory_version v
           LEFT JOIN memory m ON m.key = v.key
          WHERE v.type = ?1
          GROUP BY v.key
          ORDER BY v.key",
    )?;
    let rows = stmt.query_map(params![MEMORY], |r| {
        Ok(HistoryIndex {
            key: r.get(0)?,
            versions: r.get(1)?,
            last_written: r.get(2)?,
            orphan: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod erase_tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = crate::config::Paths::from_root(dir.path());
        let conn = crate::db::open(&paths).expect("store");
        (dir, conn)
    }

    #[test]
    fn an_orphans_history_is_the_only_copy_until_it_is_erased() {
        let (_dir, conn) = store();
        let mem = crate::store::Memory {
            key: "a-key".into(),
            title: "t".into(),
            body: "the body".into(),
            ..Default::default()
        };
        crate::store::add(&conn, &mem).expect("add");
        crate::store::delete(&conn, "a-key", None).expect("delete");
        assert_eq!(orphan_keys(&conn).unwrap(), vec!["a-key".to_string()]);
        assert!(!history(&conn, "a-key").unwrap().is_empty());

        let (keys, versions) = erase_orphans(&conn).unwrap();
        assert_eq!(keys, 1, "one orphan key");
        assert!(versions >= 1, "and the versions that held the text");
        assert!(orphan_keys(&conn).unwrap().is_empty(), "the pile is gone");
        assert!(history(&conn, "a-key").unwrap().is_empty());
    }

    #[test]
    fn a_live_key_is_not_an_orphan() {
        let (_dir, conn) = store();
        let mem = crate::store::Memory {
            key: "still-here".into(),
            title: "t".into(),
            body: "body".into(),
            ..Default::default()
        };
        crate::store::add(&conn, &mem).expect("add");
        assert!(
            orphan_keys(&conn).unwrap().is_empty(),
            "a live row is not history"
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryMatch {
    pub key: String,
    pub n: i64,
    pub written_at: String,
    pub orphan: bool,
    pub snippet: String,
}

fn terms(phrase: &str) -> Result<String> {
    let parts: Vec<String> = phrase
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(crate::error::Error::Invalid(
            "the phrase has no searchable terms".into(),
        ));
    }
    if let Some(short) = parts.iter().find(|t| t.len() < 2) {
        return Err(crate::error::Error::Invalid(format!(
            "`{short}` is a single character: a one-letter term is not a word search. Use a longer \
             fragment, or `pattern` over `memory_search` for a literal substring."
        )));
    }
    Ok(parts
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" AND "))
}

pub fn search(conn: &Connection, phrase: &str, limit: usize) -> Result<Vec<HistoryMatch>> {
    search_visible(conn, phrase, limit, |_| true)
}

pub fn search_visible<F>(
    conn: &Connection,
    phrase: &str,
    limit: usize,
    visible: F,
) -> Result<Vec<HistoryMatch>>
where
    F: Fn(&str) -> bool,
{
    let expr = terms(phrase)?;
    let mut stmt = conn.prepare(
        "SELECT h.key, h.n, v.written_at, (m.id IS NULL) AS orphan,
                snippet(history_fts, 0, '[', ']', '…', 12)
           FROM history_fts h
           JOIN memory_version v ON v.key = h.key AND v.n = h.n AND v.type = ?1
           LEFT JOIN memory m ON m.key = h.key
          WHERE history_fts MATCH ?2
          ORDER BY rank",
    )?;
    let mut rows = stmt.query(params![MEMORY, expr])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let hit = HistoryMatch {
            key: r.get(0)?,
            n: r.get(1)?,
            written_at: r.get(2)?,
            orphan: r.get(3)?,
            snippet: r.get(4)?,
        };
        if visible(&hit.key) {
            out.push(hit);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod history_search_tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = crate::config::Paths::from_root(dir.path());
        let conn = crate::db::open(&paths).expect("store");
        (dir, conn)
    }

    #[test]
    fn a_deleted_bodys_words_are_findable_and_then_erasable() {
        let (_d, conn) = store();
        let mem = crate::store::Memory {
            key: "a-note".into(),
            title: "t".into(),
            body: "the widget calibration constant is zebracorn".into(),
            ..Default::default()
        };
        crate::store::add(&conn, &mem).expect("add");
        let mut patch = crate::store::Patch {
            key: Some("a-note".into()),
            body: Some("rewritten, the constant moved".into()),
            ..Default::default()
        };
        crate::store::merge(&conn, &patch).expect("merge");
        crate::store::delete(&conn, "a-note", None).expect("delete");
        patch.key = None;

        let hits = search(&conn, "zebracorn", 10).expect("search");
        assert_eq!(hits.len(), 1, "the old words are found: {hits:?}");
        assert_eq!(hits[0].key, "a-note");
        assert!(
            hits[0].orphan,
            "and the row is gone, so this is the only copy"
        );
        assert!(hits[0].snippet.contains("zebracorn"), "{}", hits[0].snippet);

        assert!(
            crate::index::fts::search(&conn, "zebracorn", 10)
                .expect("live fts")
                .is_empty()
        );

        erase(&conn, "a-note").expect("erase");
        assert!(
            search(&conn, "zebracorn", 10).expect("search").is_empty(),
            "after erase the words are gone — the index follows the history"
        );
    }
}

#[cfg(test)]
mod history_visibility_tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = crate::config::Paths::from_root(dir.path());
        let conn = crate::db::open(&paths).expect("store");
        conn.execute_batch(
            "INSERT INTO pen (name, kind) VALUES ('stet','self'), ('cline','self');",
        )
        .expect("pens");
        (dir, conn)
    }

    #[test]
    fn the_audience_is_applied_before_the_cap() {
        let (_d, conn) = store();
        for key in ["secret-a", "secret-b"] {
            let m = crate::store::Memory {
                key: key.into(),
                title: "private".into(),
                body: "zebracorn appears in a private note".into(),
                audience: "pen:stet".into(),
                asserted_by: Some("stet".to_string()),
                ..Default::default()
            };
            crate::store::add(&conn, &m).expect("add");
            crate::store::delete(&conn, key, Some("stet")).expect("delete");
        }
        let public = crate::store::Memory {
            key: "public-c".into(),
            title: "public".into(),
            body: "zebracorn appears in a public note".into(),
            asserted_by: Some("stet".to_string()),
            ..Default::default()
        };
        crate::store::add(&conn, &public).expect("add");
        crate::store::delete(&conn, "public-c", Some("stet")).expect("delete");

        let other = crate::store::audience::Reader::pen("cline");
        let hits = search_visible(&conn, "zebracorn", 1, |key| {
            other.sees(&class_of(&conn, key))
        })
        .expect("search");
        assert_eq!(
            hits.len(),
            1,
            "the visible row is found, not the private ones"
        );
        assert_eq!(hits[0].key, "public-c");
    }

    fn class_of(conn: &Connection, key: &str) -> String {
        read_latest(conn, key)
            .ok()
            .flatten()
            .and_then(|s| {
                s.get("audience")
                    .and_then(|a| a.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "public".into())
    }

    #[test]
    fn a_one_character_term_is_refused_rather_than_silently_empty() {
        let (_d, conn) = store();
        let err = search(&conn, "a", 5).unwrap_err().to_string();
        assert!(err.contains("single character"), "{err}");
    }
}
