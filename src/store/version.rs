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

    let n: i64 = tx.query_row(
        "SELECT COALESCE(MAX(n), 0) + 1 FROM memory_version WHERE type = ?1 AND key = ?2",
        params![MEMORY, key],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO memory_version (type, key, n, encoding, note, content)
         VALUES (?1, ?2, ?3, 'json', ?4, ?5)",
        params![MEMORY, key, n, note, content],
    )?;
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
    Ok(conn.execute(
        "DELETE FROM memory_version WHERE type = ?1 AND key = ?2",
        params![MEMORY, key],
    )?)
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
