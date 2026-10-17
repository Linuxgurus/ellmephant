//! The write/read path over `memory` and its derived indexes (P4).

pub mod claim;
pub mod codec;
pub mod dupes;
pub mod encode;
pub mod links;
pub mod mail;
pub mod maintain;
pub mod project;
pub mod slug;
pub mod snapshot;
pub mod stats;
pub mod tags;
pub mod version;

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::embed::{VecSpec, projection};
use crate::error::{Error, Result};
use crate::index;

/// One atomic memory: the storage *and* embedding unit. `Default` is the draft baseline the
/// write path fills in (`id`/`encode_hash`/timestamps are derived on insert).
///
/// `title`, `body` and `tags` are stored literally; `encode_hash` is the re-embed trigger for the
/// single document embedding (`key` + `title` + `body` — tags are *filing*, not meaning).
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
    /// WHO asserted this row's current state, as the store resolved it — never as the caller typed
    /// it. `anonymous` when the connection proved nothing. Stamped, not an argument.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub asserted_by: Option<String>,
    /// WHERE the information came from, as the caller DECLARED it: a URL, a file, a command,
    /// "my inference". A promise, not a proof — which is why it is a separate column from
    /// `asserted_by`: a relayer names the truth's home without becoming the truth.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub origin: Option<String>,
    /// `sha256(model|dim|embed_document)`; re-embed the row only when this changes.
    /// [`ENCODE_PENDING`] while a queued job still owes this row a vector.
    #[serde(default)]
    pub encode_hash: String,
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
    /// The LIVE state's number: this key's superseded states (`memory_version.n`), plus one. A key
    /// with no history is `1`; a key that does not exist has no version at all.
    ///
    /// ★ THIS IS THE CAS TOKEN. A read-modify-write is unsafe while the store only versions (two
    /// writers can both read, both write, and last-write-wins loses one silently — the bank bug).
    /// Pass this back as `expected_version` and the write is REFUSED instead, so the caller retries
    /// with fresh numbers. Derived on read; never stored on the row.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub version: Option<i64>,
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

/// The value `encode_hash` carries while a queued job still owes this row a vector.
///
/// A real hash is never this string, so the equality tests that decide `needs_embed` keep meaning
/// what they say: `stored != computed` is true while the sentinel is in place, which is exactly the
/// fact that a job is owed.
pub const ENCODE_PENDING: &str = "encode_pending";

/// `sha256(model|dim|text)` — the per-field re-embed trigger (SPEC §15 #7).
///
/// Takes a [`VecSpec`] rather than a loaded embedder: both inputs are *config*, so fingerprinting
/// a document must never be a reason to load 1.2 GB of weights. The bytes hashed are unchanged —
/// this is a signature change only, so no stored `encode_hash` moves (pinned by
/// `row_hash_output_is_pinned`).
pub fn row_hash(spec: &VecSpec, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(spec.model_id.as_bytes());
    h.update(b"|");
    h.update(spec.dim.to_string().as_bytes());
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
pub(crate) fn write_links(conn: &Connection, id: i64, key: &str, body: &str) -> Result<()> {
    conn.execute("DELETE FROM memory_link WHERE from_id = ?1", params![id])?;
    for (kind, target) in links::extract_links(body) {
        if kind == links::Kind::Memory && target == key {
            continue; // a self-link carries no navigation value
        }
        conn.execute(
            "INSERT OR IGNORE INTO memory_link (from_id, to_key, to_type, rel)
             VALUES (?1, ?2, ?3, 'see')",
            params![id, target, kind.as_str()],
        )?;
    }
    Ok(())
}

/// Upsert a memory by `key`, keeping the row, its tags, its links and its **queue entry** in one
/// transaction (SPEC §7, PLAN §6 P4). The document is *scheduled*, not embedded: no encoder is
/// touched here, and the vector is written later by the encode worker (`ENCODE-WORKER.md`).
///
/// Returns the rowid and the resolved key (the slug may differ from `mem.key` when it was
/// derived from the title), so the caller never has to re-read the row.
///
/// A non-empty `mem.created`/`mem.updated` is honoured; empty strings fall back to
/// `datetime('now')`, which is what the CLI/MCP write path passes.
///
/// No project opinion lives here at all: membership is a TAG on the memory (`prj:<key>`), so it is
/// just another entry in `mem.tags` and this function never has to know about projects.
pub fn add(conn: &Connection, spec: &VecSpec, mem: &Memory) -> Result<(i64, String)> {
    let (id, key, _, _) = add_impl(conn, spec, mem, None, None)?;
    Ok((id, key))
}

/// The largest body a memory may carry, in chars.
///
/// Past this the encoder stops being a write's cost and becomes an outage: measured on a real store
/// at roughly 1.3-8.5 s per KB of body, a 16 KB body held the whole store for 51 s and a 1 MB one
/// for minutes, because a write holds the session for its duration. A memory is one topic, not a
/// payload — a body this long is several memories wearing one key.
pub const MAX_BODY_CHARS: usize = 16_384;

/// Refuse a body past [`MAX_BODY_CHARS`]. Every write path passes through [`add_impl`], so this is
/// a property of the STORE rather than of whichever door asked.
fn check_body(body: &str) -> Result<()> {
    let len = body.chars().count();
    if len > MAX_BODY_CHARS {
        return Err(Error::TooLarge(format!(
            "body too long: {len} chars (limit {MAX_BODY_CHARS}); a memory is one topic, not a \
             payload — split it across memories and link them"
        )));
    }
    Ok(())
}

/// The shared body of [`add`] and [`merge`]. It also reports whether the document actually moved
/// (and so whether a job was queued), which [`add`]'s own callers do not need.
///
/// **The embed that used to live here is gone**, and with it the encoder: an inference under the
/// session lock is what made one slow write a store-wide stall. The row and its queue entry are
/// written in one transaction instead, so the work cannot be lost.
///
/// **It also does not create the vec table**, which it used to. That table's width is a runtime
/// property of the *embedder*, and a write no longer holds one — so the only code that can honestly
/// create it is the code holding a vector ([`crate::store::encode::settle_encoded`], via
/// `index::vec::ensure`). Deriving the width from config here would pin the store to whatever the
/// config said rather than what the encoder actually emits.
fn add_impl(
    conn: &Connection,
    spec: &VecSpec,
    mem: &Memory,
    note: Option<&str>,
    expected: Option<i64>,
) -> Result<(i64, String, bool, Option<i64>)> {
    check_body(&mem.body)?;
    let prepared = prepare(conn, spec, mem, note)?;
    let queued = prepared.needs_embed();
    // ★ IMMEDIATE, not the default DEFERRED. A deferred transaction that READS (the compare) and
    // then WRITES must promote its lock, and in WAL that promotion fails with `SQLITE_BUSY_SNAPSHOT`
    // whenever a rival committed in between — an error the caller cannot tell apart from a conflict.
    // Taking the write lock first makes "compare, then write" ONE serialized unit, which is the whole
    // point of a CAS. Writes are short; the wait is a few milliseconds.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // ★ AND THE COMPARE IS INSIDE THE TRANSACTION, or it is theatre: a check outside would let a
    // rival commit between the read and the write, which is exactly the lost update this exists to
    // prevent.
    if let Some(want) = expected {
        let current = live_version(&tx, &prepared.key)?;
        if current != want {
            return Ok((0, prepared.key, false, Some(current)));
        }
    }
    let id = apply(&tx, mem, &prepared)?;
    tx.commit()?;
    Ok((id, prepared.key, queued, None))
}

/// The LIVE state's version: `MAX(n)` of the superseded states, plus one. A key that does not exist
/// is `0` — not `1` — so a CREATE can be CAS'd too, and two creators cannot both think they are
/// writing the first state.
pub fn live_version(conn: &Connection, key: &str) -> Result<i64> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory WHERE key = ?1)",
        params![key],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(0);
    }
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(n), 0) + 1 FROM memory_version WHERE type = ?1 AND key = ?2",
        params![version::MEMORY, key],
        |r| r.get(0),
    )?)
}

/// A **partial** write to one memory: every field is optional and `None` means *leave it alone*.
///
/// This is the difference from [`add`], which is whole-row — an omitted field there is written
/// empty. So the store can tell "not mentioned" from "cleared": `Some("")` empties `source`, while
/// an empty `title`/`body` is refused, because a memory needs both.
#[derive(Debug, Clone, Default)]
pub struct Patch {
    /// Which memory to write. Absent, the key is derived from `title`.
    pub key: Option<String>,
    /// A new title. May not be empty.
    pub title: Option<String>,
    /// A new body. May not be empty.
    pub body: Option<String>,
    /// New provenance. `Some("")` clears it — empty is a fact here.
    pub source: Option<String>,
    /// The store stamps this from the connection's proven identity; a caller may not set it.
    pub asserted_by: Option<String>,
    /// Where the information came from, declared by the caller. `None` leaves the stored value.
    pub origin: Option<String>,
    /// New tags. `Some(vec![])` clears them.
    pub tags: Option<Vec<String>>,
    /// Why THIS change is being made. Not a field of the memory: it rides beside the version the
    /// write supersedes, so a later reader learns the intent without reconstructing it. It creates
    /// no version on its own — only a DOCUMENT change does (see `store::version`).
    pub version_note: Option<String>,
    /// ★ THE CAS TOKEN: write only if the memory is still at this version (the `version` field a
    /// `memory_get` returned). `None` is the old last-write-wins behaviour.
    ///
    /// `0` means "I believe this key does not exist yet", so a create can be raced too. A mismatch
    /// writes NOTHING and reports the current version, which is the difference between a lost update
    /// and a retry.
    pub expected_version: Option<i64>,
}

/// What [`merge`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// The row that was written (a slug derived from the title if no key was given).
    pub key: String,
    /// True when the key did not exist and a row was inserted.
    pub created: bool,
    /// True when the embedded document changed, so an embed was **queued** (not performed — see
    /// `ENCODE-WORKER.md`). Named for what the store did, which is all it can honestly promise now.
    pub queued: bool,
    /// `Some(current)` means the write was REFUSED because `expected_version` did not match — nothing
    /// was written, and `current` is the version to re-read and retry against.
    pub conflict: Option<i64>,
}

/// Upsert ONE memory from a partial [`Patch`]: `None` keeps the stored value, `Some` replaces it.
/// A key with no row is created, and then `title` and `body` are both required — there is nothing
/// to keep.
///
/// Queues an embed only when the embedded document (`key` + title + body) actually changes: a tag or
/// a source is *filing*, not *meaning* (SPEC §9).
/// The tag that makes a memory writable only under a LIVE LEASE.
pub const PROTECTED: &str = "protected";

fn is_protected(tags: &[String]) -> bool {
    tags.iter().any(|t| t.eq_ignore_ascii_case(PROTECTED))
}

/// ★ THE LEASE IS THE CONTROL; THE TAG SAYS WHICH ROWS NEED ONE.
///
/// A `protected` memory is writable only by a caller holding a live lease on its key. A write that
/// would ADD or REMOVE the tag passes the same gate, so a stranger can neither ratchet a memory shut
/// nor quietly unlock one.
///
/// The lease must be taken deliberately (`claim_acquire`), which is what makes an edit visible
/// afterwards: the claim table is the write audit for the memories that matter. Pair this with
/// `ELLMEPHANT_LEASERS`, or a persuaded model simply leases the key itself and the gate is theatre.
fn guard_protected(conn: &Connection, key: &str, was: &[String], patch: &Patch) -> Result<()> {
    let becomes = patch.tags.as_ref().is_some_and(|t| is_protected(t));
    if !is_protected(was) && !becomes {
        return Ok(());
    }
    let Some(claim) = crate::store::claim::get(conn, key)? else {
        return Err(Error::Invalid(format!(
            "`{key}` is PROTECTED: take the lease first (`claim_acquire name={key}`), write, then \
             release. A protected memory is writable only under a live lease."
        )));
    };
    if !claim.live {
        return Err(Error::Invalid(format!(
            "`{key}` is PROTECTED and its lease lapsed at {} (held by `{}`). Take it again.",
            claim.expires_at, claim.owner
        )));
    }
    // A PROVEN writer must BE the holder. An unproven one cannot be told from any other client, and
    // the lease it holds is already the strongest statement available: someone took it on purpose.
    if let Some(writer) = patch
        .asserted_by
        .as_deref()
        .filter(|w| !claim.owner.eq_ignore_ascii_case(w))
    {
        return Err(Error::Invalid(format!(
            "`{key}` is PROTECTED and leased by `{}` until {}; `{writer}` does not hold it.",
            claim.owner, claim.expires_at
        )));
    }
    Ok(())
}

/// A delete has no patch to inspect, so it only asks: is a live lease held on this key?
fn guard_protected_delete(conn: &Connection, key: &str) -> Result<()> {
    let Some(mem) = get(conn, key)? else {
        return Ok(());
    };
    if !is_protected(&mem.tags) {
        return Ok(());
    }
    match crate::store::claim::get(conn, key)? {
        Some(claim) if claim.live => Ok(()),
        Some(claim) => Err(Error::Invalid(format!(
            "`{key}` is PROTECTED and its lease lapsed at {} (held by `{}`). Take it again.",
            claim.expires_at, claim.owner
        ))),
        None => Err(Error::Invalid(format!(
            "`{key}` is PROTECTED: take the lease first (`claim_acquire name={key}`), delete, then \
             release."
        ))),
    }
}

pub fn merge(conn: &Connection, spec: &VecSpec, patch: &Patch) -> Result<MergeOutcome> {
    let probe = Memory {
        key: patch.key.clone().unwrap_or_default(),
        title: patch.title.clone().unwrap_or_default(),
        ..Default::default()
    };
    let key = resolve_key(conn, &probe)?;
    let existing = get(conn, &key)?;
    // A `protected` row (or a write that would make one) needs a live lease on its own key.
    let was_tags = existing
        .as_ref()
        .map(|m| m.tags.clone())
        .unwrap_or_default();
    guard_protected(conn, &key, &was_tags, patch)?;

    let (mem, created) = match existing {
        Some(old) => {
            let title = patch.title.clone().unwrap_or_else(|| old.title.clone());
            let body = patch.body.clone().unwrap_or_else(|| old.body.clone());
            if title.trim().is_empty() {
                return Err(Error::Invalid(
                    "`title` cannot be empty; omit it to leave the stored title alone".into(),
                ));
            }
            if body.trim().is_empty() {
                return Err(Error::Invalid(
                    "`body` cannot be empty; omit it to leave the stored body alone".into(),
                ));
            }
            let source = match &patch.source {
                Some(s) => (!s.trim().is_empty()).then(|| s.clone()),
                None => old.source.clone(),
            };
            (
                Memory {
                    key: key.clone(),
                    title,
                    body,
                    tags: patch.tags.clone().unwrap_or_else(|| old.tags.clone()),
                    source,
                    // The writer is this connection, not whoever wrote it last; `None` keeps the
                    // stored attribution so an internal write cannot launder it into anonymity.
                    asserted_by: patch
                        .asserted_by
                        .clone()
                        .or_else(|| old.asserted_by.clone()),
                    origin: patch.origin.clone().or_else(|| old.origin.clone()),
                    ..Default::default()
                },
                false,
            )
        }
        None => {
            let title = patch.title.clone().unwrap_or_default();
            let body = patch.body.clone().unwrap_or_default();
            if title.trim().is_empty() || body.trim().is_empty() {
                return Err(Error::Invalid(
                    "a new memory needs both a `title` and a `body`; an existing key can be patched \
                     field by field"
                        .into(),
                ));
            }
            (
                Memory {
                    key: key.clone(),
                    title,
                    body,
                    tags: patch.tags.clone().unwrap_or_default(),
                    source: patch.source.clone().filter(|s| !s.trim().is_empty()),
                    asserted_by: patch.asserted_by.clone(),
                    origin: patch.origin.clone().filter(|s| !s.trim().is_empty()),
                    ..Default::default()
                },
                true,
            )
        }
    };

    let (_id, key, queued, conflict) = add_impl(
        conn,
        spec,
        &mem,
        patch.version_note.as_deref(),
        patch.expected_version,
    )?;
    Ok(MergeOutcome {
        key,
        // A refused write created nothing, whatever the pre-write read believed.
        created: created && conflict.is_none(),
        queued,
        conflict,
    })
}

/// A write resolved *outside* the transaction: the key, the document's hash, and the existing row it
/// will upsert. Resolving the key and the hash without a model call is what keeps the write path
/// encoder-free.
pub(crate) struct Prepared {
    pub(crate) key: String,
    encode_hash: String,
    existing: Option<(i64, String)>,
    /// Why this write is happening, carried to the snapshot it may take. Not part of the document,
    /// so it changes no hash and queues no embed.
    note: Option<String>,
}

impl Prepared {
    /// Whether the document needs (re-)embedding.
    ///
    /// `stored != computed` is true while [`ENCODE_PENDING`] sits in the column, which is exactly the
    /// fact that a job is owed — so the sentinel needs no special case here.
    pub(crate) fn needs_embed(&self) -> bool {
        self.existing
            .as_ref()
            .is_none_or(|(_, h)| h != &self.encode_hash)
    }
}

/// Resolve a write without opening a transaction (reads only, no model call).
pub(crate) fn prepare(
    conn: &Connection,
    spec: &VecSpec,
    mem: &Memory,
    note: Option<&str>,
) -> Result<Prepared> {
    let key = resolve_key(conn, mem)?;
    let doc_text = projection::embed_document(&key, &mem.title, &mem.body);
    let encode_hash = row_hash(spec, &doc_text);
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, encode_hash FROM memory WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(Prepared {
        key,
        encode_hash,
        existing,
        note: note.map(str::to_string),
    })
}

/// Apply a prepared write inside the caller's transaction.
///
/// It does **not** write the vector, and it does **not** stamp the real `encode_hash`: that happens in
/// [`crate::store::encode::settle_encoded`], beside the vector it describes. What it DOES do is mark
/// the row as owing one — a write that schedules a job leaves `ENCODE_PENDING` in the column. So
/// pending-ness is a STATE a reader can see, not an inference from a hash mismatch, and the value
/// explains itself at the moment it is read.
pub(crate) fn apply(tx: &Connection, mem: &Memory, prepared: &Prepared) -> Result<i64> {
    let pending = prepared.needs_embed();
    let id = match prepared.existing {
        Some((id, _)) => {
            if pending {
                // Snapshot what this write is about to replace, in the SAME transaction: a row and
                // its history must not be able to disagree (migration 0011). Only a DOCUMENT change
                // earns a version, which is exactly what `pending` means — a tag-only write is
                // filing, not meaning.
                version::snapshot(tx, id, &prepared.key, prepared.note.as_deref())?;
            }
            update_row(tx, id, mem, pending)?;
            id
        }
        None => insert_row(tx, &prepared.key, mem)?,
    };
    write_links(tx, id, &prepared.key, &mem.body)?;
    if pending {
        encode::enqueue(
            tx,
            &prepared.key,
            &prepared.encode_hash,
            encode::INTERACTIVE,
        )?;
    }
    Ok(id)
}

/// The `UPDATE` half of [`add`]: the key already exists. `pending` leaves [`ENCODE_PENDING`] in
/// `encode_hash` when this write scheduled a job, and leaves the column alone otherwise.
fn update_row(tx: &Connection, id: i64, mem: &Memory, pending: bool) -> Result<()> {
    tx.execute(
        "UPDATE memory
            SET title = ?2, body = ?3, tags = ?4, source = ?5,
                asserted_by = COALESCE(?9, asserted_by),
                origin = COALESCE(?10, origin),
                encode_hash = CASE WHEN ?7 THEN ?8 ELSE encode_hash END,
                updated = COALESCE(NULLIF(?6, ''), datetime('now'))
          WHERE id = ?1",
        params![
            id,
            mem.title,
            mem.body,
            tags::encode(&mem.tags),
            mem.source,
            mem.updated,
            pending,
            ENCODE_PENDING,
            mem.asserted_by,
            mem.origin
        ],
    )?;
    Ok(())
}

/// The `INSERT` half of [`add`]: a new key; returns its rowid.
///
/// `encode_hash` starts at [`ENCODE_PENDING`] — a fresh row always owes an encode — and the worker
/// settles it with the real hash of the vector it writes.
fn insert_row(tx: &Connection, key: &str, mem: &Memory) -> Result<i64> {
    tx.execute(
        "INSERT INTO memory (key, title, body, tags, source, encode_hash, created, updated, asserted_by, origin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                 COALESCE(NULLIF(?7, ''), datetime('now')),
                 COALESCE(NULLIF(?8, ''), datetime('now')),
                 COALESCE(?9, 'anonymous'), ?10)",
        params![
            key,
            mem.title,
            mem.body,
            tags::encode(&mem.tags),
            mem.source,
            ENCODE_PENDING,
            mem.created,
            mem.updated,
            mem.asserted_by,
            mem.origin
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Delete a memory by key. `memory_link` follows via FK cascade; the vector is dropped explicitly, and
/// so is the key's queue entry. Returns whether a row was removed.
pub fn delete(conn: &Connection, key: &str) -> Result<bool> {
    guard_protected_delete(conn, key)?;
    let tx = conn.unchecked_transaction()?;
    // The queue entry goes whatever the memory row does: an entry naming a row that no longer exists
    // is a document that can never be embedded, so leaving one behind is a leak the worker can only
    // rediscover. Both halves ride in one transaction, so neither undoes the other.
    encode::forget(&tx, key)?;
    let id: Option<i64> = tx
        .query_row("SELECT id FROM memory WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(id) = id {
        // A delete SUPERSEDES the state the memory is in, so it snapshots it first — exactly as an
        // edit does, and in the same transaction. Without this, a deleted key's newest version would
        // be some earlier state and the content it actually held would be the one thing versioning
        // did not keep; with it, `erase` is the store's single true destroyer.
        version::snapshot(&tx, id, key, None)?;
        tx.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
        index::vec::delete(&tx, id)?;
    }
    tx.commit()?;
    Ok(id.is_some())
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

/// The canonical column list shared by `get_by_id` and `all`. `version` is DERIVED, not stored: the
/// count of states this key has superseded, plus one — the token a caller passes back as
/// `expected_version` (see [`Patch::expected_version`]).
const MEMORY_COLUMNS: &str = "id, key, title, body, tags, source, encode_hash, created, updated, \
     visits, times_useful, \
     (SELECT COALESCE(MAX(n), 0) + 1 FROM memory_version \
       WHERE type = 'memory' AND key = memory.key) AS version, \
     asserted_by, origin";

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
        encode_hash: r.get(6)?,
        created: r.get(7)?,
        updated: r.get(8)?,
        visits: r.get(9)?,
        times_useful: r.get(10)?,
        version: Some(r.get(11)?),
        asserted_by: r.get(12)?,
        origin: r.get(13)?,
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

    #[test]
    fn hash_changes_with_model_dim_and_text() {
        let a = VecSpec::new("model-a", 8);
        let b = VecSpec::new("model-b", 8);
        let wide = VecSpec::new("model-a", 16);
        assert_ne!(row_hash(&a, "x"), row_hash(&wide, "x"), "dim must matter");
        assert_ne!(row_hash(&a, "x"), row_hash(&b, "x"), "model must matter");
        assert_ne!(row_hash(&a, "x"), row_hash(&a, "y"), "text must matter");
        assert_eq!(row_hash(&a, "x"), row_hash(&a, "x"), "must be stable");
        assert_eq!(row_hash(&a, "x").len(), 64, "sha256 hex is 64 chars");
    }

    /// `row_hash` takes a [`VecSpec`] now instead of a loaded embedder. The hash is written into
    /// every row of a live store, so a changed value would re-embed all of it — silently, and visible
    /// only as a slow rebuild. This pins the exact bytes the old `sha256(model|dim|text)` produced
    /// (`printf 'm|3|x' | shasum -a 256`), so the refactor cannot move it.
    #[test]
    fn row_hash_output_is_pinned() {
        assert_eq!(
            row_hash(&VecSpec::new("m", 3), "x"),
            "37ea0bd227f4e7bd235c7f93353dfb9d87c811d7a6d4017a43c089cdda3dd26f"
        );
    }
}
