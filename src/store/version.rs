//! Memory **versions**: the state a document change replaces (migration `0011`).
//!
//! The source half had no undo. A row that was wrong once and then corrected kept only the
//! correction — so every hygiene rule built on that fact is a workaround: "FLAG rather than
//! rewrite", because a rewrite destroys the only evidence of what was there. A version is that
//! evidence, kept.
//!
//! WHAT IS STORED: a full JSON snapshot of the source row (key, title, body, tags, source, created,
//! updated) as it stood immediately BEFORE a change to its DOCUMENT — `title` + `body`, the fields
//! the embedder sees. Snapshots rather than deltas: one row reads as a complete state, with nothing
//! to replay. `n` counts states per key, so `n = 1` is the first state that was replaced.
//!
//! WHAT IS DELIBERATELY NOT STORED, none of it an oversight:
//!   * nothing for an INSERT — nothing was replaced;
//!   * nothing for a TAG or SOURCE write — filing is not meaning, which is exactly the
//!     the title/body comparison the write path already makes;
//!   * no vector, ever: a version is reached by ADDRESS and must be unable to be found as current;
//!   * no cascade from `memory` (there is no FK), so history survives a delete — which makes erasure
//!     a POLICY ([`erase`]) rather than an accident of the schema;
//!   * nothing when `memory_merge` absorbs a row: a merge is lossless by construction, so there is
//!     nothing to preserve;
//!   * a DELETE **does** snapshot: it supersedes the state the memory is in, so the newest version of
//!     a deleted key IS the content it held. A delete therefore loses nothing — it only silences an
//!     address — which makes [`erase`] the store's single true destroyer.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// The document kind a version belongs to. A project is planned work and is not embedded, so it has
/// no document to change; if its prose ever gets one, it inherits this table under a new kind.
pub const MEMORY: &str = "memory";

/// One stored version *without its body*: enough to choose a version before paying for one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    /// 1-based, per (kind, key). `1` is the first state that was replaced.
    pub n: i64,
    /// When the snapshot was taken. The state inside it may be much older — its own `created` and
    /// `updated` ride in `content`.
    pub written_at: String,
    /// How `content` is encoded. `json` today.
    pub encoding: String,
    /// Why the change was made, when the writer said so.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
    /// Size of `content` in bytes, so a reader can pick without fetching bodies.
    pub bytes: i64,
}

/// Snapshot the row AS IT STANDS, inside the caller's transaction.
///
/// Called immediately before the update that supersedes it, so the read here is exactly the state
/// being replaced. `note` records why, when the writer knows.
pub(crate) fn snapshot(tx: &Connection, id: i64, key: &str, note: Option<&str>) -> Result<i64> {
    let (k, title, body, tags, source, created, updated): (
        String,
        String,
        String,
        String,
        Option<String>,
        String,
        String,
    ) = tx.query_row(
        "SELECT key, title, body, tags, source, created, updated FROM memory WHERE id = ?1",
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
            ))
        },
    )?;

    // Self-describing: the fields the row held, with tags as an ARRAY rather than the stored
    // comma-joined string, so a reader needs no knowledge of the encoding.
    let content = serde_json::json!({
        "key": k,
        "title": title,
        "body": body,
        "tags": tags.split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
        "source": source,
        "created": created,
        "updated": updated,
    })
    .to_string();

    // The version says who wrote the state it holds: the row's `edited_by` at this instant, i.e.
    // before the change that is about to supersede it (#109).
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
    // ★ THE WORDS, INDEXED WITH THE SNAPSHOT (soma/ellmephant#115). Insert-only: a version is
    // immutable, so this index never needs an UPDATE path — and the body is already in hand here,
    // so nothing has to parse the JSON back out later.
    if history_fts_ready(tx) {
        tx.execute(
            "INSERT INTO history_fts (body, key, n) VALUES (?1, ?2, ?3)",
            params![body, key, n],
        )?;
    }
    Ok(n)
}

/// Every version of one key, oldest first, WITHOUT bodies.
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

/// Is the words-over-history index present? A fixture that builds a partial schema (or a store
/// created before 0042) must keep working: the index is an addition, not a precondition.
fn history_fts_ready(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'history_fts'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// One version's snapshot, as stored — the row as it WAS, not a reconstruction of it.
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

/// Total versions held and the bytes they occupy — what `stats` reports.
pub fn count(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT count(*), COALESCE(sum(length(content)), 0) FROM memory_version",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// How many versions one key holds — the question a delete leaves behind: *what did I not remove?*
/// The NEWEST snapshot of a key, whole. Used to class the history of a key whose live row is gone:
/// the last thing it said about who may read it is the only class it has.
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

/// The ERASURE act: drop every version of one key. The row itself is untouched.
///
/// This exists so that "the store keeps a copy of what you asked it to remove" is never true by
/// accident. It is narrow on purpose — history is the only thing it can delete.
pub fn erase(conn: &Connection, key: &str) -> Result<usize> {
    // ★ ERASE REACHES THE INDEX IN THE SAME STATEMENT (soma/ellmephant#115): the test for this is
    // the EFFECT — a phrase from an erased history must stop being findable — not the return value.
    if history_fts_ready(conn) {
        conn.execute("DELETE FROM history_fts WHERE key = ?1", params![key])?;
    }
    Ok(conn.execute(
        "DELETE FROM memory_version WHERE type = ?1 AND key = ?2",
        params![MEMORY, key],
    )?)
}

/// ★ THE PILE THAT GROWS BY ITSELF (soma/brain#82): every key whose memory row is GONE but whose
/// versions still hold its text. A delete snapshots the row, so this list grows with every hygiene
/// pass — rows go down, bytes do not. Nothing reads an orphan by accident: its address is dead, and
/// only `memory_history`/`memory_get version=N` can reach it.
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

/// Erase the history of every key whose row is gone. One transaction, so a sweep is all or nothing.
/// Returns `(keys, versions)`. THIS DESTROYS TEXT — the caller gates it (see the tool's `confirm`).
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

/// One key's roll-up for the history INDEX: what you can see without fetching a single body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryIndex {
    /// The address the history belongs to.
    pub key: String,
    /// How many versions it holds.
    pub versions: i64,
    /// When the most recent snapshot was taken.
    pub last_written: String,
    /// True when the `memory` row is GONE: this history is then the ONLY copy of that content.
    pub orphan: bool,
}

/// Every key that has versions, with a count and the last write — the INDEX of history.
///
/// It answers the question a delete raises: *where did it go?* A key with no row still has an
/// address, and its versions are still reachable one at a time by [`read`]. Discovery is by KEY, not
/// by meaning (a version is never embedded), and that is enough in practice because keys are
/// descriptive slugs — the key list IS the search index for history.
///
/// `orphan` is the flag that matters most: it marks history whose row is gone. For an orphan,
/// history IS the source, so a retention pass must treat it differently — prune orphans LAST or not
/// at all, because pruning one is a real deletion of the only copy.
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
        // A delete SNAPSHOTS the row, so the text survives as a version and the key becomes an orphan.
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

/// ★ THE THIRD AXIS: exact words over HISTORY (soma/ellmephant#115).
///
/// One hit is a PAST state, not a current row: `key`, which version `n`, when it was written, and
/// whether the memory still exists. A reader that gets this must not mistake it for the document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryMatch {
    pub key: String,
    pub n: i64,
    pub written_at: String,
    /// The memory row is GONE — this is the only copy of that text.
    pub orphan: bool,
    /// The matching words in context.
    pub snippet: String,
}

/// Quote every term and OR them, so a sentence cannot become an FTS syntax error.
fn terms(phrase: &str) -> String {
    phrase
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .filter(|t| t.len() > 1)
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Search the words every version ever held. Never fused into `memory_search`: a hit here is a past
/// state, and a ranking that mixed the two would put deleted text in a live page.
pub fn search(conn: &Connection, phrase: &str, limit: usize) -> Result<Vec<HistoryMatch>> {
    let expr = terms(phrase);
    if expr.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT h.key, h.n, v.written_at, (m.id IS NULL) AS orphan,
                snippet(history_fts, 0, '[', ']', '…', 12)
           FROM history_fts h
           JOIN memory_version v ON v.key = h.key AND v.n = h.n AND v.type = ?1
           LEFT JOIN memory m ON m.key = h.key
          WHERE history_fts MATCH ?2
          ORDER BY rank
          LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![MEMORY, expr, limit as i64], |r| {
        Ok(HistoryMatch {
            key: r.get(0)?,
            n: r.get(1)?,
            written_at: r.get(2)?,
            orphan: r.get(3)?,
            snippet: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
        // ★ #115 acceptance 1-3: write, rewrite, delete; the phrase is found, named as a PAST state
        // and an orphan; and after erase it is GONE — tested by the effect, not the return value.
        let (_d, conn) = store();
        let mem = crate::store::Memory {
            key: "a-note".into(),
            title: "t".into(),
            body: "the widget calibration constant is zebracorn".into(),
            ..Default::default()
        };
        crate::store::add(&conn, &mem).expect("add");
        // A rewrite snapshots the old body into history.
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

        // The same phrase is NOT in the live word index — that is the whole point of a third axis.
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
