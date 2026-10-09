//! The write/read path over `memory` and its derived indexes (P4).

pub mod links;
pub mod maintain;
pub mod slug;
pub mod tags;

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::embed::{Embedder, projection};
use crate::error::Result;
use crate::index;

/// `memory.type` — how a memory is meant to be used (SPEC §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// How to find or derive something.
    Signpost,
    /// A rule to act on.
    Dictate,
    /// A plain fact.
    #[default]
    Fact,
    /// A sparse pointer used for routing only.
    Profile,
}

impl MemoryType {
    /// The string stored in the `type` column.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryType::Signpost => "signpost",
            MemoryType::Dictate => "dictate",
            MemoryType::Fact => "fact",
            MemoryType::Profile => "profile",
        }
    }

    /// Parse the `type` column; anything unrecognised falls back to [`Fact`](Self::Fact).
    pub fn parse(s: &str) -> Self {
        match s {
            "signpost" => MemoryType::Signpost,
            "dictate" => MemoryType::Dictate,
            "profile" => MemoryType::Profile,
            _ => MemoryType::Fact,
        }
    }
}

/// One atomic memory: the storage *and* embedding unit. `Default` is the draft baseline
/// the write path fills in (`id`/`hash`/timestamps are derived on insert).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Memory {
    /// Rowid; `None` before insert and omitted from export (local/derived).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<i64>,
    /// Global-unique slug; identity and link target.
    pub key: String,
    /// Single bucket (`infra`, `projects`, ...).
    pub scope: String,
    /// Usage class.
    #[serde(rename = "type", default)]
    pub type_: MemoryType,
    /// Salient one-liner.
    pub title: String,
    /// Full, verifiable body.
    pub body: String,
    /// Provenance: command/path/repo/quote.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<String>,
    /// `sha256(model|dim|embed_text)`; re-embed only when this changes.
    #[serde(default)]
    pub hash: String,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Creation timestamp (SQLite `datetime('now')`).
    #[serde(default)]
    pub created: String,
    /// Last-update timestamp.
    #[serde(default)]
    pub updated: String,
}

/// `sha256(model|dim|embed_text)` — the re-embed trigger (SPEC §15 #7).
pub fn row_hash(embedder: &dyn Embedder, embed_text: &str) -> String {
    let mut h = Sha256::new();
    h.update(embedder.model_id().as_bytes());
    h.update(b"|");
    h.update(embedder.dim().to_string().as_bytes());
    h.update(b"|");
    h.update(embed_text.as_bytes());
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

/// Resolve the row's `key`: an explicit key is taken as-is (upsert by key); otherwise
/// slugify `scope + title` and add a `-2`/`-3` suffix on collision (SPEC §4 #11).
fn resolve_key(conn: &Connection, mem: &Memory) -> Result<String> {
    if !mem.key.trim().is_empty() {
        return Ok(slug::slugify(&mem.key));
    }
    let base = slug::slugify(&format!("{} {}", mem.scope, mem.title));
    Ok(slug::unique_slug(&base, &taken_variants(conn, &base)?))
}

/// Replace the tag links for `id` with the normalised set on `mem` (tags are shared
/// vocabulary, so the `tag` rows themselves are only ever upserted).
fn write_tags(conn: &Connection, id: i64, mem: &Memory) -> Result<()> {
    conn.execute("DELETE FROM memory_tag WHERE memory_id = ?1", params![id])?;
    for raw in &mem.tags {
        let Some(tag) = tags::normalize(raw) else {
            continue;
        };
        conn.execute("INSERT OR IGNORE INTO tag (name) VALUES (?1)", params![tag])?;
        conn.execute(
            "INSERT OR IGNORE INTO memory_tag (memory_id, tag_id)
             SELECT ?1, id FROM tag WHERE name = ?2",
            params![id, tag],
        )?;
    }
    Ok(())
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
/// transaction (SPEC §7, PLAN §6 P4). Re-embeds only when the row hash changes.
///
/// A non-empty `mem.created`/`mem.updated` is honoured (import preserves timestamps);
/// empty strings fall back to `datetime('now')`, which is what the CLI/MCP write path
/// passes.
pub fn add(conn: &Connection, embedder: &dyn Embedder, mem: &Memory) -> Result<i64> {
    let prepared = prepare(conn, embedder, mem)?;
    // Create the vec table before the tx so its DDL is not inside it.
    index::vec::ensure(conn, embedder.dim())?;
    // Embed before the tx: inference is slow (SPEC §13) and must not hold the write lock.
    let vector = if prepared.needs_embed() {
        Some(embedder.embed_one(&prepared.text)?)
    } else {
        None
    };
    let tx = conn.unchecked_transaction()?;
    let id = apply(&tx, mem, &prepared, vector.as_deref())?;
    tx.commit()?;
    Ok(id)
}

/// A write resolved *outside* the transaction: the key, embed projection, hash and the
/// existing row it will upsert. Lets the caller embed before taking the write lock.
pub(crate) struct Prepared {
    pub(crate) key: String,
    pub(crate) text: String,
    hash: String,
    existing: Option<(i64, String)>,
    dim: i64,
    model: String,
}

impl Prepared {
    /// Whether the stored hash differs, i.e. the text needs (re-)embedding.
    pub(crate) fn needs_embed(&self) -> bool {
        self.existing.as_ref().is_none_or(|(_, h)| h != &self.hash)
    }
}

/// Resolve a write without opening a transaction (reads only, no model call).
pub(crate) fn prepare(
    conn: &Connection,
    embedder: &dyn Embedder,
    mem: &Memory,
) -> Result<Prepared> {
    let key = resolve_key(conn, mem)?;
    let text = projection::embed_text(&mem.title, &mem.body);
    let hash = row_hash(embedder, &text);
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, hash FROM memory WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(Prepared {
        key,
        text,
        hash,
        existing,
        dim: embedder.dim() as i64,
        model: embedder.model_id().to_string(),
    })
}

/// Apply a prepared write inside the caller's transaction. `vector` must be present
/// exactly when [`Prepared::needs_embed`] is true.
pub(crate) fn apply(
    tx: &Connection,
    mem: &Memory,
    prepared: &Prepared,
    vector: Option<&[f32]>,
) -> Result<i64> {
    let id = match prepared.existing {
        Some((id, _)) => {
            update_row(tx, id, mem, &prepared.hash, prepared.dim, &prepared.model)?;
            id
        }
        None => insert_row(
            tx,
            &prepared.key,
            mem,
            &prepared.hash,
            prepared.dim,
            &prepared.model,
        )?,
    };
    write_tags(tx, id, mem)?;
    write_links(tx, id, &prepared.key, &mem.body)?;
    if let Some(vector) = vector {
        index::vec::upsert(tx, id, &mem.scope, vector)?;
    }
    Ok(id)
}

/// The `UPDATE` half of [`add`]: the key already exists.
fn update_row(
    tx: &Connection,
    id: i64,
    mem: &Memory,
    hash: &str,
    dim: i64,
    model: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE memory
            SET scope = ?2, type = ?3, title = ?4, body = ?5, source = ?6,
                hash = ?7, model = ?8, dim = ?9,
                updated = COALESCE(NULLIF(?10, ''), datetime('now'))
          WHERE id = ?1",
        params![
            id,
            mem.scope,
            mem.type_.as_str(),
            mem.title,
            mem.body,
            mem.source,
            hash,
            model,
            dim,
            mem.updated
        ],
    )?;
    Ok(())
}

/// The `INSERT` half of [`add`]: a new key; returns its rowid.
fn insert_row(
    tx: &Connection,
    key: &str,
    mem: &Memory,
    hash: &str,
    dim: i64,
    model: &str,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO memory (key, scope, type, title, body, source, hash, model, dim,
                             created, updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                 COALESCE(NULLIF(?10, ''), datetime('now')),
                 COALESCE(NULLIF(?11, ''), datetime('now')))",
        params![
            key,
            mem.scope,
            mem.type_.as_str(),
            mem.title,
            mem.body,
            mem.source,
            hash,
            model,
            dim,
            mem.created,
            mem.updated
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Delete a memory by key. `memory_fts`/`memory_tag`/`memory_link` follow via triggers
/// and FK cascade; the vector is removed explicitly. Returns whether a row was removed.
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

/// Map one `memory` row (the canonical column list) to a `Memory`; tags come separately.
fn row_to_memory(r: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: Some(r.get(0)?),
        key: r.get(1)?,
        scope: r.get(2)?,
        type_: MemoryType::parse(&r.get::<_, String>(3)?),
        title: r.get(4)?,
        body: r.get(5)?,
        source: r.get(6)?,
        hash: r.get(7)?,
        tags: Vec::new(),
        created: r.get(8)?,
        updated: r.get(9)?,
    })
}

/// The canonical column list shared by `get_by_id` and `all`.
const MEMORY_COLUMNS: &str = "id, key, scope, type, title, body, source, hash, created, updated";

/// Fetch one memory (with tags) by rowid — the hydration step of the read path (P5).
pub fn get_by_id(conn: &Connection, id: i64) -> Result<Option<Memory>> {
    let row = conn
        .query_row(
            &format!("SELECT {MEMORY_COLUMNS} FROM memory WHERE id = ?1"),
            params![id],
            row_to_memory,
        )
        .optional()?;

    let Some(mut mem) = row else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT t.name FROM tag t
           JOIN memory_tag mt ON mt.tag_id = t.id
          WHERE mt.memory_id = ?1 ORDER BY t.name",
    )?;
    let tags = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
    mem.tags = tags.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(mem))
}

/// Every memory (with tags), ordered by `key` — the export path, in two queries.
pub fn all(conn: &Connection) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(&format!("SELECT {MEMORY_COLUMNS} FROM memory ORDER BY key"))?;
    let mut memories = stmt
        .query_map([], row_to_memory)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let id_to_index: HashMap<i64, usize> = memories
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.id.map(|id| (id, i)))
        .collect();
    let mut stmt = conn.prepare(
        "SELECT mt.memory_id, t.name FROM memory_tag mt JOIN tag t ON t.id = mt.tag_id
          ORDER BY t.name",
    )?;
    let pairs = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for pair in pairs {
        let (id, name) = pair?;
        if let Some(&i) = id_to_index.get(&id) {
            memories[i].tags.push(name);
        }
    }
    Ok(memories)
}

/// Every key in the store, alphabetical — the REPL's tab-completion vocabulary.
pub fn keys(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT key FROM memory ORDER BY key")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
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

    #[test]
    fn memory_type_round_trips() {
        for t in [
            MemoryType::Signpost,
            MemoryType::Dictate,
            MemoryType::Fact,
            MemoryType::Profile,
        ] {
            assert_eq!(MemoryType::parse(t.as_str()), t);
        }
        assert_eq!(MemoryType::parse("nonsense"), MemoryType::Fact);
    }
}
