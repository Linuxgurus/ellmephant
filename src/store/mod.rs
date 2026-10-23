//! The write/read path over `memory` and its derived indexes (P4).

pub mod audience;
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

use crate::error::{Error, Result};
use crate::index;

/// One atomic memory: the storage *and* embedding unit. `Default` is the draft baseline the
/// write path fills in (`id`/timestamps are derived on insert).
///
/// `title`, `body` and `tags` are stored literally; a change to title or body is what queues a single
/// document embedding (`key` + `title` + `body` — tags are *filing*, not meaning).
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
    ///
    /// ★ WRITE-ONCE. It is set when the row is created and never changes: a later write names itself
    /// in [`Memory::edited_by`] instead. A tag pass may not launder authorship (#107).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub asserted_by: Option<String>,
    /// WHO wrote this row's CURRENT state — the moving fact, and the one that answers "who wrote
    /// this" (a snapshot carries the same fact for the state it holds). Every document write sets
    /// it; a tag-only edit leaves it alone, because filing is not writing (#107).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub edited_by: Option<String>,
    /// WHO IS ANSWERABLE for this row — the current judgement, not the origin of the words. `None`
    /// means nobody is: the commons (`owner IS NULL`), where `protected` and `private` do not apply
    /// and any pen may write or adopt. A finger never owns; its parent does (#109).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub owner: Option<String>,
    /// WHERE the information came from, as the caller DECLARED it: a URL, a file, a command,
    /// "my inference". A promise, not a proof — which is why it is a separate column from
    /// `asserted_by`: a relayer names the truth's home without becoming the truth.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub origin: Option<String>,
    /// Who may be shown this row: `public`, or `pen:<x>`. The READ tier (#54/#55) — never a tag,
    /// because a tag is editable by any caller and an audience must not be.
    #[serde(
        default = "audience::default_audience",
        skip_serializing_if = "is_public"
    )]
    pub audience: String,
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

fn is_public(a: &str) -> bool {
    a == audience::PUBLIC
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
pub fn add(conn: &Connection, mem: &Memory) -> Result<(i64, String)> {
    let (id, key, _, _) = add_impl(conn, mem, None, None)?;
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
    mem: &Memory,
    note: Option<&str>,
    expected: Option<i64>,
) -> Result<(i64, String, bool, Option<i64>)> {
    check_body(&mem.body)?;
    let prepared = prepare(conn, mem, note)?;
    let queued = prepared.changes_document(mem);
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
    /// Set the row PRIVATE to the writing pen (`true`) or public (`false`). `None` leaves the stored
    /// audience alone.
    ///
    /// ★ The value is not a name: the store derives the audience from the PROVEN identity, so a
    /// caller cannot make a row private to somebody else, or publish a row it did not write.
    pub private: Option<bool>,
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

/// The audience is not a preference — it is a boundary, and boundaries are not editable by the party
/// they exclude. A row private to `pen:stet` may only be re-classified by `stet`, or by the owner's
/// explicit forensic path (which is a different act, recorded).
fn guard_audience(existing: Option<&Memory>, patch: &Patch) -> Result<()> {
    let Some(old) = existing else {
        return Ok(());
    };
    if old.audience == audience::PUBLIC {
        return Ok(());
    }
    let Some(private) = patch.private else {
        return Ok(());
    };
    let already_private = audience::for_write(true, patch.asserted_by.as_deref()).ok();
    if private && already_private.as_deref() == Some(old.audience.as_str()) {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "`{}` is private to another pen: its audience is not yours to change. A row you cannot read \
         is not a row you may publish.",
        old.key
    )))
}

pub fn merge(conn: &Connection, patch: &Patch) -> Result<MergeOutcome> {
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
    // An audience belongs to the pen that set it: a caller may not publish, or privatise, a row it
    // could not read. Without this, `private` would be a lock whose key hangs beside the door.
    guard_audience(existing.as_ref(), patch)?;

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
                    // ★ AUTHORSHIP IS WRITE-ONCE (#107). A patch is a write, not a claim about who
                    // asserted the document: the stored author is carried through untouched, and the
                    // writer names itself in `edited_by` — so re-filing a row cannot launder
                    // its provenance, and the move is visible without a version.
                    asserted_by: old.asserted_by.clone(),
                    edited_by: patch.asserted_by.clone().or_else(|| old.edited_by.clone()),
                    origin: patch.origin.clone().or_else(|| old.origin.clone()),
                    audience: match patch.private {
                        None => old.audience.clone(),
                        Some(p) => audience::for_write(
                            p,
                            patch.asserted_by.as_deref().or(old.asserted_by.as_deref()),
                        )?,
                    },
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
                    edited_by: patch.asserted_by.clone(),
                    origin: patch.origin.clone().filter(|s| !s.trim().is_empty()),
                    audience: audience::for_write(
                        patch.private == Some(true),
                        patch.asserted_by.as_deref(),
                    )?,
                    ..Default::default()
                },
                true,
            )
        }
    };

    let (_id, key, queued, conflict) = add_impl(
        conn,
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

/// A write resolved *outside* the transaction: the key, and the existing document it will upsert.
///
/// ★ NO HASH (2026-10-21). The old `encode_hash` was a stamp describing a DERIVED index, stored on a
/// truth row — and it is what produced two silent no-ops (a reset queue that drained embedding
/// nothing; a hash claiming "indexed" over a torn index). The queue now holds the rendered text and
/// its own settle guard, so the row needs no encoding state at all: "is this a document change?" is
/// answered by comparing the stored title/body, which is the question an embed actually depends on.
pub(crate) struct Prepared {
    pub(crate) key: String,
    existing: Option<(i64, String, String)>,
    /// Why this write is happening, carried to the snapshot it may take. Not part of the document,
    /// so it changes no hash and queues no embed.
    note: Option<String>,
}

impl Prepared {
    /// Whether this write changes the DOCUMENT — i.e. whether a vector is owed.
    ///
    /// A tag or source edit is filing, not meaning, so it queues nothing. Comparison is against the
    /// stored title and body, which is exactly what the renderer reads (plus the key, which cannot
    /// change under an upsert).
    pub(crate) fn changes_document(&self, mem: &Memory) -> bool {
        match &self.existing {
            None => true,
            Some((_, title, body)) => title != &mem.title || body != &mem.body,
        }
    }
}

/// Resolve a write without opening a transaction (reads only, no model call).
pub(crate) fn prepare(conn: &Connection, mem: &Memory, note: Option<&str>) -> Result<Prepared> {
    let key = resolve_key(conn, mem)?;
    let existing: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, title, body FROM memory WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(Prepared {
        key,
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
    let pending = prepared.changes_document(mem);
    let id = match prepared.existing {
        Some((id, _, _)) => {
            if pending {
                // Snapshot what this write is about to replace, in the SAME transaction: a row and
                // its history must not be able to disagree (migration 0011). Only a DOCUMENT change
                // earns a version, which is exactly what `pending` means — a tag-only write is
                // filing, not meaning.
                version::snapshot(tx, id, &prepared.key, prepared.note.as_deref())?;
            }
            update_row(tx, id, mem)?;
            id
        }
        None => insert_row(tx, &prepared.key, mem)?,
    };
    write_links(tx, id, &prepared.key, &mem.body)?;
    if pending {
        // The queue carries the TEXT, so the render happens here — at the write, where the kind is
        // known — and the worker never needs to know what a memory is.
        encode::enqueue_memory(
            tx,
            &prepared.key,
            &mem.title,
            &mem.body,
            encode::PRIORITY_LIVE,
        )?;
    }
    Ok(id)
}

/// The root pen: the estate itself, which is where terraform stands (a provisioning act carries the
/// root's authority, not a human's). A root has no parent to retire it, so the root is its own
/// custodian — a pen that goes bad at 03:00 has to be stoppable by somebody (#109).
pub const ROOT_CUSTODIAN: &str = "estate";

/// The exact row for a name — no parent resolution. Ownership rules need both this and
/// [`owner_pen_id`]: a finger acts as itself but owns as its parent.
fn exact_pen_id(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT id FROM pen WHERE name = ?1", params![name], |r| {
            r.get(0)
        })
        .optional()?)
}

/// Is `ancestor` anywhere above `of` in the tree? Walks the parent chain, bounded: a cycle is a bug,
/// not a shape, and it must not hang a request.
fn is_ancestor(conn: &Connection, ancestor: i64, of: i64) -> Result<bool> {
    let mut cur = of;
    for _ in 0..64 {
        let parent: Option<i64> = conn
            .query_row(
                "SELECT parent_id FROM pen WHERE id = ?1",
                params![cur],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match parent {
            Some(p) if p == ancestor => return Ok(true),
            Some(p) => cur = p,
            None => return Ok(false),
        }
    }
    Ok(false)
}

/// The `pen` row that a name OWNS as: itself, or its parent when the name is a finger
/// (`stet/finger-3` → `stet`). A finger is a mechanism by which one self does more work; it is not
/// a party to ownership (#109).
fn owner_pen_id(tx: &Connection, name: Option<&str>) -> Result<Option<i64>> {
    let Some(name) = name else { return Ok(None) };
    Ok(tx
        .query_row(
            // KIND decides, not the name: a finger owns as its parent, everything else owns as
            // itself. A name is an address; a permission read from a string is a permission anyone
            // who can register a name can forge (Stet, review round 3).
            "SELECT CASE WHEN author.kind = 'finger' THEN COALESCE(author.parent_id, author.id)
                         ELSE author.id END
               FROM pen author WHERE author.name = ?1",
            params![name],
            |r| r.get(0),
        )
        .optional()?)
}

/// Append to the responsibility ledger. Content history lives in `memory_version`; this table
/// answers who took, released or handed on a row, and when — never what was written.
fn log_pen_event(
    tx: &Connection,
    key: &str,
    action: &str,
    pen: Option<i64>,
    from: Option<i64>,
    to: Option<i64>,
    detail: Option<&str>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO pen_event (key, action, pen_id, from_pen, to_pen, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![key, action, pen, from, to, detail],
    )?;
    Ok(())
}

/// The `UPDATE` half of [`add`]: the key already exists. `pending` leaves [`ENCODE_PENDING`] in
/// `encode_hash` when this write scheduled a job, and leaves the column alone otherwise.
fn update_row(tx: &Connection, id: i64, mem: &Memory) -> Result<()> {
    tx.execute(
        "UPDATE memory
            SET title = ?2, body = ?3, tags = ?4, source = ?5,
                asserted_by = COALESCE(?7, asserted_by),
                origin = COALESCE(?8, origin),
                audience = ?9,
                edited_by = COALESCE(?10, ?7, edited_by),
                updated = COALESCE(NULLIF(?6, ''), datetime('now'))
          WHERE id = ?1",
        params![
            id,
            mem.title,
            mem.body,
            tags::encode(&mem.tags),
            mem.source,
            mem.updated,
            mem.asserted_by,
            mem.origin,
            audience::effective(&mem.audience),
            mem.edited_by
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
        "INSERT INTO memory (key, title, body, tags, source, created, updated, asserted_by, origin, audience,
                            edited_by, owner)
         VALUES (?1, ?2, ?3, ?4, ?5,
                 COALESCE(NULLIF(?6, ''), datetime('now')),
                 COALESCE(NULLIF(?7, ''), datetime('now')),
                 COALESCE(?8, 'anonymous'), ?9, ?10,
                 COALESCE(?11, ?8, 'anonymous'), ?12)",
        params![
            key,
            mem.title,
            mem.body,
            tags::encode(&mem.tags),
            mem.source,
            mem.created,
            mem.updated,
            mem.asserted_by,
            mem.origin,
            audience::effective(&mem.audience),
            mem.edited_by,
            owner_pen_id(tx, mem.asserted_by.as_deref())?
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Take responsibility for a row. `owner` moves to the adopting pen; a finger's parent is what
/// actually owns, so adopting as `stet/finger-3` is adopting as `stet` (#109).
pub fn adopt(conn: &Connection, key: &str, pen: &str, force: bool) -> Result<()> {
    // You may not adopt what you cannot read: adopting somebody else's private row would be a way
    // to take it by noticing it exists (#109, authorization).
    let audience: Option<String> = conn
        .query_row(
            "SELECT audience FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?;
    let audience = audience.ok_or_else(|| Error::NotFound(key.to_string()))?;
    if audience != audience::PUBLIC && audience != format!("pen:{pen}") {
        return Err(Error::Invalid(format!(
            "{key} is private to {audience}; a pen cannot adopt a row it cannot read"
        )));
    }
    let to = owner_pen_id(conn, Some(pen))?.ok_or_else(|| {
        Error::Invalid(format!(
            "no pen named {pen}; the pen registry has no such row"
        ))
    })?;
    // An OWNED row may be taken only by an ancestor, and only deliberately (#109, James): a pen's
    // work is not up for grabs by a sibling, or a stranger, who happens to notice it.
    let from: Option<i64> = conn
        .query_row(
            "SELECT owner FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if let Some(owner_id) = from {
        let caller_id = exact_pen_id(conn, pen)?
            .ok_or_else(|| Error::Invalid(format!("no pen named {pen}")))?;
        if caller_id == owner_id {
            return Ok(()); // already yours; nothing to take
        }
        if !is_ancestor(conn, caller_id, owner_id)? {
            return Err(Error::Invalid(format!(
                "{pen} may not adopt {key}: only an ancestor of its owner may take an owned row"
            )));
        }
        if !force {
            return Err(Error::Invalid(format!(
                "{key} already has an owner; taking it needs force=true"
            )));
        }
    }
    let n = conn.execute(
        "UPDATE memory SET owner = ?2 WHERE key = ?1",
        params![key, to],
    )?;
    if n == 0 {
        return Err(Error::NotFound(key.to_string()));
    }
    log_pen_event(conn, key, "adopt", Some(to), from, Some(to), None)
}

/// Release a row: nobody is answerable for it any more. By the commons rule (#109) an ownerless row
/// also loses `protected` and `private` — a disclaim is therefore a PUBLICATION, and is recorded.
pub fn disclaim(conn: &Connection, key: &str, pen: &str, publish: bool) -> Result<()> {
    let who =
        exact_pen_id(conn, pen)?.ok_or_else(|| Error::Invalid(format!("no pen named {pen}")))?;
    // The owner may release its own row; any ancestor may release it for them. Nobody else.
    let owner_id: Option<i64> = conn
        .query_row(
            "SELECT owner FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    // Any ANCESTOR may release a descendant's row (James), not just the parent: a grandparent
    // skipping a generation is the same act. The commons belongs to nobody, so nobody has to be
    // asked to release it.
    let allowed = match owner_id {
        Some(id) => who == id || is_ancestor(conn, who, id)?,
        None => true,
    };
    if !allowed {
        return Err(Error::Invalid(format!(
            "{pen} may not disclaim {key}: only its owner or one of that owner's ancestors may"
        )));
    }
    let from = owner_id;
    let was_private = conn
        .query_row(
            "SELECT audience FROM memory WHERE key = ?1",
            params![key],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|a| a != audience::PUBLIC)
        .unwrap_or(false);
    // ★ PUBLISHING ANOTHER PEN'S PRIVATE ROW IS NOT A DISCLAIM (Stet, review round 3). The private
    // tier exists because the AUTHOR chose the audience; an ancestor releasing the row would publish
    // words that were not its own, and nothing un-publishes. So private -> public must be named:
    // `publish=true`, a `publish` row in the ledger (not `disclaim`), and the affected pen is the
    // event's `from_pen`, so it can find the act by asking about itself.
    if was_private && !publish {
        return Err(Error::Invalid(format!(
            "{key} is private; releasing it would publish another pen's words — pass publish=true to do that deliberately"
        )));
    }
    let n = conn.execute(
        "UPDATE memory SET owner = NULL, audience = ?2 WHERE key = ?1",
        params![key, audience::PUBLIC],
    )?;
    if n == 0 {
        return Err(Error::NotFound(key.to_string()));
    }
    log_pen_event(
        conn,
        key,
        if was_private { "publish" } else { "disclaim" },
        Some(who),
        from,
        None,
        Some(if was_private {
            "published on disclaim: the row was private and is now readable by every pen"
        } else {
            "released"
        }),
    )
}

/// Hand a row from one pen to another. Both must be real pens; a finger resolves to its parent.
pub fn transfer(conn: &Connection, key: &str, from: &str, to: &str) -> Result<()> {
    let from_id = owner_pen_id(conn, Some(from))?
        .ok_or_else(|| Error::Invalid(format!("no pen named {from}")))?;
    let to_id = owner_pen_id(conn, Some(to))?
        .ok_or_else(|| Error::Invalid(format!("no pen named {to}")))?;
    let n = conn.execute(
        "UPDATE memory SET owner = ?2 WHERE key = ?1",
        params![key, to_id],
    )?;
    if n == 0 {
        return Err(Error::NotFound(key.to_string()));
    }
    log_pen_event(
        conn,
        key,
        "transfer",
        Some(from_id),
        Some(from_id),
        Some(to_id),
        None,
    )
}

/// What happens to a retired pen's ROWS — the first of the two choices (#109, round 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rows {
    /// `owner` moves to the parent: the rows survive, under the parent's responsibility.
    Inherit,
    /// `owner` -> NULL: the commons. Private/protected drop, so a private row is PUBLISHED.
    Disclaim,
    /// The rows are gone; the keys stay as tombstones and the act is recorded.
    Delete,
}

/// What survives of the RECORD — the second choice. Independent of the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum History {
    /// Tombstone; versions listed, bodies fetchable, memorial note on the ledger.
    Memorialize,
    /// Tombstone; content not meant to be read again — versions go.
    Forget,
    /// `memory_history_erase`: the store's only true destroyer.
    Purge,
}

/// One ledger row: when, what, from whom, to whom — the responsibility story of a key (#109).
pub type PenEvent = (String, String, Option<String>, Option<String>);

/// What a retirement would do, computed without doing it.
#[derive(Debug, Clone, Serialize)]
pub struct RetirePreview {
    pub pen: String,
    pub parent: Option<String>,
    pub rows: i64,
    pub private_rows: i64,
}

fn pen_row(conn: &Connection, name: &str) -> Result<(i64, Option<i64>, Option<String>)> {
    conn.query_row(
        "SELECT id, parent_id, ceased_at FROM pen WHERE name = ?1",
        params![name],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

/// Count what retiring `pen` would touch. Read-only; safe to call before deciding.
pub fn retire_preview(conn: &Connection, pen: &str) -> Result<RetirePreview> {
    let (id, parent_id, _) = pen_row(conn, pen)?;
    let rows: i64 = conn.query_row(
        "SELECT count(*) FROM memory WHERE owner = ?1",
        params![id],
        |r| r.get(0),
    )?;
    let private_rows: i64 = conn.query_row(
        "SELECT count(*) FROM memory WHERE owner = ?1 AND audience <> ?2",
        params![id, audience::PUBLIC],
        |r| r.get(0),
    )?;
    let parent: Option<String> = match parent_id {
        Some(pid) => conn
            .query_row("SELECT name FROM pen WHERE id = ?1", params![pid], |r| {
                r.get(0)
            })
            .optional()?,
        None => None,
    };
    Ok(RetirePreview {
        pen: pen.to_string(),
        parent,
        rows,
        private_rows,
    })
}

/// Retire a pen: it ceases to be, and its rows and record go where the caller says.
///
/// NO DEFAULT, deliberately (#109): a retirement that does not say what happens to the rows is how
/// ghosts are made — the pen is gone, the rows still name it, and `who_can` starts answering about a
/// pen that officially does not exist.
///
/// Not wrapped in one transaction: `delete` opens its own. A failure part-way leaves the pen live and
/// the ledger holding the events that did happen, which is the recoverable direction.
pub fn retire(
    conn: &Connection,
    pen: &str,
    caller: &str,
    rows: Rows,
    history: History,
) -> Result<RetirePreview> {
    let preview = retire_preview(conn, pen)?;
    let caller_id = exact_pen_id(conn, caller)?
        .ok_or_else(|| Error::Invalid(format!("no pen named {caller}")))?;
    let (id, parent_id, ceased_at) = pen_row(conn, pen)?;
    if ceased_at.is_some() {
        return Err(Error::Invalid(format!("{pen} is already retired")));
    }
    // Authorization: a parent retires its child. A ROOT has no parent, so the root custodian may —
    // a pen that goes bad at 03:00 must be stoppable by somebody (#109).
    let permitted = match parent_id {
        Some(pid) => caller_id == pid || is_ancestor(conn, caller_id, pid)?,
        None => caller == ROOT_CUSTODIAN,
    };
    if !permitted {
        return Err(Error::Invalid(match parent_id {
            Some(_) => format!("{caller} may not retire {pen}: only an ancestor may"),
            None => format!("{caller} may not retire root pen {pen}: only {ROOT_CUSTODIAN} may"),
        }));
    }
    let parent = match (rows, parent_id) {
        (Rows::Inherit, None) => {
            return Err(Error::Invalid(format!(
                "{pen} has no parent: `inherit` needs somewhere for the rows to go (use disclaim or delete)"
            )));
        }
        (Rows::Inherit, Some(pid)) => Some(pid),
        _ => None,
    };

    let keys: Vec<String> = {
        let mut stmt = conn.prepare("SELECT key FROM memory WHERE owner = ?1 ORDER BY key")?;

        stmt.query_map(params![id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    for key in &keys {
        match rows {
            Rows::Inherit => {
                let to = parent.expect("checked above");
                conn.execute(
                    "UPDATE memory SET owner = ?2 WHERE key = ?1",
                    params![key, to],
                )?;
                log_pen_event(
                    conn,
                    key,
                    "transfer",
                    Some(id),
                    Some(id),
                    Some(to),
                    Some("inherited on retirement"),
                )?;
            }
            Rows::Disclaim => {
                let was_private: bool = conn
                    .query_row(
                        "SELECT audience FROM memory WHERE key = ?1",
                        params![key],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .map(|a| a != audience::PUBLIC)
                    .unwrap_or(false);
                conn.execute(
                    "UPDATE memory SET owner = NULL, audience = ?2 WHERE key = ?1",
                    params![key, audience::PUBLIC],
                )?;
                log_pen_event(
                    conn,
                    key,
                    "disclaim",
                    Some(id),
                    Some(id),
                    None,
                    Some(if was_private {
                        "disclaimed on retirement: private dropped, a disclaimed row is public"
                    } else {
                        "disclaimed on retirement"
                    }),
                )?;
            }
            Rows::Delete => {
                log_pen_event(
                    conn,
                    key,
                    "delete",
                    Some(id),
                    Some(id),
                    None,
                    Some("deleted on retirement"),
                )?;
                delete(conn, key)?;
            }
        }
        match history {
            History::Memorialize => {}
            History::Forget | History::Purge => {
                version::erase(conn, key)?;
            }
        }
    }

    conn.execute(
        "UPDATE pen SET ceased_at = datetime('now') WHERE id = ?1",
        params![id],
    )?;
    log_pen_event(
        conn,
        "",
        "retire",
        Some(id),
        Some(id),
        parent,
        Some(&format!(
            "rows: {:?} ({} rows, {} private), history: {:?}",
            rows, preview.rows, preview.private_rows, history
        )),
    )?;
    Ok(preview)
}

/// The responsibility ledger for one key: who acted, on what, from whom to whom, and why.
pub fn pen_events(conn: &Connection, key: &str) -> Result<Vec<PenEvent>> {
    let mut stmt = conn.prepare(
        "SELECT at, action, (SELECT name FROM pen WHERE id = from_pen), (SELECT name FROM pen WHERE id = to_pen)
           FROM pen_event WHERE key = ?1 ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![key], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Delete a memory by key. `memory_link` follows via FK cascade; the vector is dropped explicitly, and
/// so is the key's queue entry. Returns whether a row was removed.
pub fn delete(conn: &Connection, key: &str) -> Result<bool> {
    guard_protected_delete(conn, key)?;
    let tx = conn.unchecked_transaction()?;
    // The queue entry goes whatever the memory row does: an entry naming a row that no longer exists
    // is a document that can never be embedded, so leaving one behind is a leak the worker can only
    // rediscover. Both halves ride in one transaction, so neither undoes the other.
    encode::forget(&tx, &crate::store::links::Kind::Memory.keyslug(key))?;
    let row: Option<(i64, String)> = tx
        .query_row(
            "SELECT id, audience FROM memory WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let existed = row.is_some();
    if let Some((id, audience)) = row {
        // A delete SUPERSEDES the state the memory is in, so it snapshots it first — exactly as an
        // edit does, and in the same transaction. Without this, a deleted key's newest version would
        // be some earlier state and the content it actually held would be the one thing versioning
        // did not keep; with it, `erase` is the store's single true destroyer.
        version::snapshot(&tx, id, key, None)?;
        tx.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
        // ★ A PRIVATE ROW'S VECTOR IS PURGED, NOT TOMBSTONED. The tombstone table is a DELIBERATE
        // search surface (`semantic_search(deleted=true)`), so moving a private document's vector
        // there would keep it findable by meaning — by anyone — after its row was gone, and a
        // tombstone names it. Deleting a private note must delete what points at it.
        if audience == audience::PUBLIC || audience.is_empty() {
            index::vec::delete(&tx, id)?;
        } else {
            index::vec::purge(&tx, id)?;
        }
    }
    tx.commit()?;
    Ok(existed)
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
const MEMORY_COLUMNS: &str = "id, key, title, body, tags, source, created, updated, \
     visits, times_useful, \
     (SELECT COALESCE(MAX(n), 0) + 1 FROM memory_version \
       WHERE type = 'memory' AND key = memory.key) AS version, \
     asserted_by, origin, audience, edited_by, \
     (SELECT name FROM pen WHERE pen.id = memory.owner) AS owner";

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
        created: r.get(6)?,
        updated: r.get(7)?,
        visits: r.get(8)?,
        times_useful: r.get(9)?,
        version: Some(r.get(10)?),
        asserted_by: r.get(11)?,
        origin: r.get(12)?,
        audience: r.get(13)?,
        edited_by: r.get(14)?,
        owner: r.get(15)?,
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
