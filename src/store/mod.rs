//! The write/read path over `memory` and its derived indexes (P4).

pub mod dupes;
pub mod links;
pub mod maintain;
pub mod slug;
pub mod stats;
pub mod tags;

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::embed::{Embedder, projection};
use crate::error::{Error, Result};
use crate::index;

/// One atomic memory: the storage *and* embedding unit. `Default` is the draft baseline the
/// write path fills in (`id`/`doc_hash`/timestamps are derived on insert).
///
/// `title`, `body` and `tags` are stored literally; `doc_hash` is the re-embed trigger for the
/// single document embedding (`key` + tags + `title` + `body`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Memory {
    /// Rowid; `None` before insert and omitted from serialized output (local/derived).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<i64>,
    /// Global-unique slug; identity and link target.
    pub key: String,
    /// Smart-caveman one-liner.
    pub title: String,
    /// Smart-caveman fact.
    pub body: String,
    /// Provenance: command/path/repo/quote.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<String>,
    /// `sha256(model|dim|embed_document)`; re-embed the row only when this changes.
    #[serde(default)]
    pub doc_hash: String,
    /// Aspect tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Creation timestamp (SQLite `datetime('now')`).
    #[serde(default)]
    pub created: String,
    /// Last-update timestamp.
    #[serde(default)]
    pub updated: String,
    /// Times the row was read whole (`memory_get`/`show`); runtime counter, never in the embed.
    #[serde(default)]
    pub visits: i64,
    /// Count of explicit "this helped" marks from an agent; runtime counter, never in the embed.
    #[serde(default)]
    pub times_useful: i64,
    /// Split-review verdict: `Some(true)` = one topic, `Some(false)` = a grab-bag worth
    /// splitting, `None` = not yet judged. Runtime property, never in the embed; an upsert
    /// resets it to `None` (the row changed, so re-judge it).
    #[serde(default)]
    pub coherent: Option<bool>,
}

/// One row's index entry: identity and shape, without the body (the `list` surface, SPEC §14).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    /// Slug.
    pub key: String,
    /// Salient line.
    pub title: String,
    /// Attached tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// `sha256(model|dim|text)` — the per-field re-embed trigger (SPEC §15 #7).
pub fn row_hash(embedder: &dyn Embedder, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(embedder.model_id().as_bytes());
    h.update(b"|");
    h.update(embedder.dim().to_string().as_bytes());
    h.update(b"|");
    h.update(text.as_bytes());
    crate::hex::encode(&h.finalize())
}

/// Keys that share `base` as a prefix (for slug collision avoidance). `base` is `[a-z0-9-]`,
/// so its `LIKE` pattern carries no wildcards.
fn taken_variants(conn: &Connection, base: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT key FROM memory WHERE key LIKE ?1")?;
    let rows = stmt.query_map(params![format!("{base}%")], |r| r.get::<_, String>(0))?;
    let mut set = HashSet::new();
    for row in rows {
        set.insert(row?);
    }
    Ok(set)
}

/// Resolve the row's `key`: an explicit key is taken as-is (upsert by key); otherwise slugify
/// the title and add a `-2`/`-3` suffix on collision (SPEC §4 #11).
fn resolve_key(conn: &Connection, mem: &Memory) -> Result<String> {
    if !mem.key.trim().is_empty() {
        return Ok(slug::slugify(&mem.key));
    }
    let base = slug::slugify(&mem.title);
    Ok(slug::unique_slug(&base, &taken_variants(conn, &base)?))
}

/// Replace the outgoing `[[key]]` edges for `id` with those parsed from `body`.
fn write_links(conn: &Connection, id: i64, key: &str, body: &str) -> Result<()> {
    conn.execute("DELETE FROM memory_link WHERE from_id = ?1", params![id])?;
    for target in links::extract_links(body) {
        if target == key {
            continue; // a self-link carries no navigation value
        }
        conn.execute(
            "INSERT OR IGNORE INTO memory_link (from_id, to_key, rel) VALUES (?1, ?2, 'see')",
            params![id, target],
        )?;
    }
    Ok(())
}

/// Upsert a memory by `key`, keeping the row, its tags, its links and its vector in one
/// transaction (SPEC §7, PLAN §6 P4). Re-embeds the document only when its hash changes.
///
/// Returns the rowid and the resolved key (the slug may differ from `mem.key` when it was
/// derived from the title), so the caller never has to re-read the row.
///
/// A non-empty `mem.created`/`mem.updated` is honoured; empty strings fall back to
/// `datetime('now')`, which is what the CLI/MCP write path passes.
pub fn add(conn: &Connection, embedder: &dyn Embedder, mem: &Memory) -> Result<(i64, String)> {
    let prepared = prepare(conn, embedder, mem)?;
    // Create the vec table before the tx so its DDL is not inside it.
    index::vec::ensure(conn, embedder.dim())?;
    // Embed before the tx: inference is slow (SPEC §13) and must not hold the write lock.
    let vector = if prepared.needs_embed() {
        Some(embedder.embed_one(&prepared.doc_text)?)
    } else {
        None
    };
    let tx = conn.unchecked_transaction()?;
    let id = apply(&tx, mem, &prepared, vector.as_deref())?;
    tx.commit()?;
    Ok((id, prepared.key))
}

/// A write resolved *outside* the transaction: the key, the embedded document, its hash, and
/// the existing row it will upsert. Lets the caller embed before taking the write lock.
pub(crate) struct Prepared {
    pub(crate) key: String,
    pub(crate) doc_text: String,
    doc_hash: String,
    existing: Option<(i64, String)>,
}

impl Prepared {
    /// Whether the document needs (re-)embedding.
    pub(crate) fn needs_embed(&self) -> bool {
        self.existing
            .as_ref()
            .is_none_or(|(_, h)| h != &self.doc_hash)
    }
}

/// Resolve a write without opening a transaction (reads only, no model call).
pub(crate) fn prepare(
    conn: &Connection,
    embedder: &dyn Embedder,
    mem: &Memory,
) -> Result<Prepared> {
    let key = resolve_key(conn, mem)?;
    let doc_text = projection::embed_document(&key, &mem.tags, &mem.title, &mem.body);
    let doc_hash = row_hash(embedder, &doc_text);
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, doc_hash FROM memory WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(Prepared {
        key,
        doc_text,
        doc_hash,
        existing,
    })
}

/// Apply a prepared write inside the caller's transaction. `vector` must be present exactly
/// when `needs_embed` is true.
pub(crate) fn apply(
    tx: &Connection,
    mem: &Memory,
    prepared: &Prepared,
    vector: Option<&[f32]>,
) -> Result<i64> {
    let id = match prepared.existing {
        Some((id, _)) => {
            update_row(tx, id, mem, &prepared.doc_hash)?;
            id
        }
        None => insert_row(tx, &prepared.key, mem, &prepared.doc_hash)?,
    };
    write_links(tx, id, &prepared.key, &mem.body)?;
    if let Some(vector) = vector {
        index::vec::upsert(tx, id, vector)?;
    }
    Ok(id)
}

/// The `UPDATE` half of [`add`]: the key already exists.
fn update_row(tx: &Connection, id: i64, mem: &Memory, doc_hash: &str) -> Result<()> {
    tx.execute(
        "UPDATE memory
            SET title = ?2, body = ?3, tags = ?4, source = ?5, doc_hash = ?6,
                coherent = NULL,
                updated = COALESCE(NULLIF(?7, ''), datetime('now'))
          WHERE id = ?1",
        params![
            id,
            mem.title,
            mem.body,
            tags::encode(&mem.tags),
            mem.source,
            doc_hash,
            mem.updated
        ],
    )?;
    Ok(())
}

/// The `INSERT` half of [`add`]: a new key; returns its rowid.
fn insert_row(tx: &Connection, key: &str, mem: &Memory, doc_hash: &str) -> Result<i64> {
    tx.execute(
        "INSERT INTO memory (key, title, body, tags, source, doc_hash, created, updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                 COALESCE(NULLIF(?7, ''), datetime('now')),
                 COALESCE(NULLIF(?8, ''), datetime('now')))",
        params![
            key,
            mem.title,
            mem.body,
            tags::encode(&mem.tags),
            mem.source,
            doc_hash,
            mem.created,
            mem.updated
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Delete a memory by key. `memory_link` follows via FK cascade; the vector is removed
/// explicitly. Returns whether a row was removed.
pub fn delete(conn: &Connection, key: &str) -> Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let id: Option<i64> = tx
        .query_row("SELECT id FROM memory WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(id) = id else {
        return Ok(false); // tx rolls back on drop
    };
    tx.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
    index::vec::delete(&tx, id)?;
    tx.commit()?;
    Ok(true)
}

/// The outcome of a feedback sweep (see [`record_useful_many`]): `(key, new_total)` for every key
/// that existed, and the keys that did not — both in input order.
pub type UsefulSweep = (Vec<(String, i64)>, Vec<String>);

/// Bump the `visits` counter for `key` and return the new total (telemetry on read).
/// Errors with [`Error::NotFound`] when the key is unknown.
pub fn record_visit(conn: &Connection, key: &str) -> Result<i64> {
    bump(conn, key, "visits")
}

/// Bump the `times_useful` counter for `key` and return the new total (explicit agent
/// feedback). Errors with [`Error::NotFound`] when the key is unknown.
pub fn record_useful(conn: &Connection, key: &str) -> Result<i64> {
    bump(conn, key, "times_useful")
}

/// Bump `times_useful` for many keys at once (a feedback sweep: one call, many keys). Returns
/// `(key, new_total)` for every key that existed, and the keys that did not, both in input
/// order. A missing key is not an error here — the caller reports the split. Duplicate keys each
/// count once, in place.
pub fn record_useful_many(conn: &Connection, keys: &[String]) -> Result<UsefulSweep> {
    let mut upvoted = Vec::new();
    let mut missing = Vec::new();
    for key in keys {
        match bump(conn, key, "times_useful") {
            Ok(total) => upvoted.push((key.clone(), total)),
            Err(Error::NotFound(_)) => missing.push(key.clone()),
            Err(e) => return Err(e),
        }
    }
    Ok((upvoted, missing))
}

/// Set the split-review verdict (`Some(true)` coherent, `Some(false)` a grab-bag, `None`
/// unjudged) for `key`. Returns whether the key existed. Never re-embeds: `coherent` is not
/// part of the document.
pub fn set_coherent(conn: &Connection, key: &str, coherent: Option<bool>) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE memory SET coherent = ?2 WHERE key = ?1",
        params![key, coherent],
    )?;
    Ok(updated > 0)
}

/// Increment `column` (`visits` or `times_useful`) for `key` and read it back. `column` is a
/// hard-coded literal, never user input.
fn bump(conn: &Connection, key: &str, column: &str) -> Result<i64> {
    let updated = conn.execute(
        &format!("UPDATE memory SET {column} = {column} + 1 WHERE key = ?1"),
        params![key],
    )?;
    if updated == 0 {
        return Err(Error::NotFound(key.to_string()));
    }
    let total = conn.query_row(
        &format!("SELECT {column} FROM memory WHERE key = ?1"),
        params![key],
        |r| r.get(0),
    )?;
    Ok(total)
}

/// Fetch one memory (with tags) by key.
pub fn get(conn: &Connection, key: &str) -> Result<Option<Memory>> {
    let id: Option<i64> = conn
        .query_row("SELECT id FROM memory WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?;
    match id {
        Some(id) => get_by_id(conn, id),
        None => Ok(None),
    }
}

/// The canonical column list shared by `get_by_id` and `all`.
const MEMORY_COLUMNS: &str = "id, key, title, body, tags, source, doc_hash, created, updated, visits, times_useful, coherent";

/// Map one `memory` row (the canonical column list) to a `Memory`.
fn row_to_memory(r: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    let tags: String = r.get(4)?;
    Ok(Memory {
        id: Some(r.get(0)?),
        key: r.get(1)?,
        title: r.get(2)?,
        body: r.get(3)?,
        tags: tags::decode(&tags),
        source: r.get(5)?,
        doc_hash: r.get(6)?,
        created: r.get(7)?,
        updated: r.get(8)?,
        visits: r.get(9)?,
        times_useful: r.get(10)?,
        coherent: r.get(11)?,
    })
}

/// Fetch one memory by rowid — the hydration step of the read path (P5).
pub fn get_by_id(conn: &Connection, id: i64) -> Result<Option<Memory>> {
    Ok(conn
        .query_row(
            &format!("SELECT {MEMORY_COLUMNS} FROM memory WHERE id = ?1"),
            params![id],
            row_to_memory,
        )
        .optional()?)
}

/// Every memory, ordered by `key`.
pub fn all(conn: &Connection) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(&format!("SELECT {MEMORY_COLUMNS} FROM memory ORDER BY key"))?;
    Ok(stmt
        .query_map([], row_to_memory)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every key in the store, alphabetical — the REPL's tab-completion vocabulary.
pub fn keys(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT key FROM memory ORDER BY key")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every row as a [`Summary`], ordered by `key`, never a body — the cheap index over the
/// store (the CLI's `list`).
pub fn summaries(conn: &Connection) -> Result<Vec<Summary>> {
    let mut stmt = conn.prepare("SELECT key, title, tags FROM memory ORDER BY key")?;
    let rows = stmt.query_map([], |r| {
        let tags: String = r.get(2)?;
        Ok(Summary {
            key: r.get(0)?,
            title: r.get(1)?,
            tags: tags::decode(&tags),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;

    #[test]
    fn hash_changes_with_model_dim_and_text() {
        let a = HashEmbedder::new(8);
        let b = HashEmbedder::new(16);
        assert_ne!(row_hash(&a, "x"), row_hash(&b, "x"), "dim must matter");
        assert_ne!(row_hash(&a, "x"), row_hash(&a, "y"), "text must matter");
        assert_eq!(row_hash(&a, "x"), row_hash(&a, "x"), "must be stable");
        assert_eq!(row_hash(&a, "x").len(), 64, "sha256 hex is 64 chars");
    }
}
