//! The write/read path over `memory` and its derived indexes (P4).

pub mod audience;
pub mod audit;
pub mod channel;
pub mod claim;
pub mod codec;
pub mod dupes;
pub mod encode;
pub mod links;
pub mod mail;
pub mod maintain;
pub mod project;
pub mod relation;
pub mod slug;
pub mod snapshot;
pub mod stats;
pub mod sudo;
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
    // ★ THE CREATE PATH HAS NO ACTOR FIELD, SO IT PASSES NONE (soma/ellmephant#192, Stet's review of
    // HEAD). It used to pass `mem.asserted_by`, which is the CREDIT — so `store::add` against an
    // EXISTING key was authorised as the name it credited, and on a commons row that name is the only
    // thing the visibility check has to go on. A create needs no actor (there is no row to write
    // over); an update through this door now fails closed on an owned row instead of guessing.
    let (id, key, _, _) = add_impl(conn, mem, None, None, None, None)?;
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
/// Can `caller` be SHOWN a row with this audience? Public, their own, or a descendant's.
///
/// ★ THE PRIVATE TIER'S LAW IS ABSENCE, NOT REFUSAL (found in the final adversarial pass): a row a
/// caller may not see must answer exactly as a key that was never written. Without this, a DELETE or a
/// MERGE on somebody's private row replied *"it is owned by stet"* — an existence-and-ownership
/// oracle for rows the caller cannot read.
/// ★ THE SESSIONS FINALLY HAVE A READER (2026-10-24). Until now `active_sudo` was a ledger nothing in
/// the permission path consulted: `may_read_audience` and `may_write` asked only about ANCESTRY, so a
/// pen could hold the estate for an hour and still be refused every row outside its own branch. The
/// verbs worked, the table filled, and the answer was always "you are not an ancestor" — which made
/// the whole sudo model decoration.
///
/// So: authority is `ancestry ∪ held sessions`, and the sessions are validated AT USE (see
/// [`sudo::reachable`]): a lend whose parentage has since moved grants nothing, and a released or
/// expired row stops answering on the next call. Nothing here is cached.
fn held_authority(conn: &Connection, caller: &str) -> Result<Vec<i64>> {
    Ok(sudo::reachable(conn, caller)?
        .into_iter()
        .map(|(id, _, _)| id)
        .collect())
}

fn may_read_audience(conn: &Connection, caller: &str, audience: &str) -> Result<bool> {
    // ★ ONE PREDICATE, ONE PLACE (2026-10-24). This used to hand-roll the rule; now it asks the very
    // same [`Reader`] the read paths ask, so a write's visibility check and a read cannot disagree
    // about a list audience, a `subtree:` share, or a borrowed root.
    let reader = crate::store::audience::Reader::pen(caller)
        .in_tree(conn)
        .with_borrowed(conn);
    Ok(reader.sees(audience))
}

/// May `caller` write the row at `key`? Parentage is the grant: the owner, the pen that owns on the
/// owner's behalf (a finger acts for its parent), or any ancestor of the owner.
///
/// ★ THE COST OF "A FINGER ACTS FOR ITS PARENT", stated here because this is where it is granted: a
/// hand pod serving many fingers gives EVERY one of those fingers the hand's write authority — so a
/// compromised finger writes as its parent, and if the parent is near the root it writes as the
/// estate (Stet, review of #119). That is inherent to fingers being mechanisms rather than parties,
/// not a defect in this check. The mitigation is that `asserted_by` survives the write: the damage
/// is ATTRIBUTABLE even when it was authorised, which is the property to preserve if this ever
/// changes. A row with no owner
/// is the commons, and the commons belongs to nobody — so every pen may write it (#109).
/// Does `caller` ANSWER for the row at `key` — is it the owner, the pen that owns on the owner's
/// behalf (a finger's parent), or an ancestor of the owner?
///
/// ★ THE ONE QUESTION BOTH GATES NEED (soma/ellmephant#189). `may_write` asks "may this pen write
/// it?"; `guard_audience` asks "may this pen re-classify it?". Visibility is a third question and
/// answers neither: the pen that wrote a row can be locked out of reading it, and it is still the pen
/// that answers for it.
fn answers_for(conn: &Connection, key: &str, caller: Option<&str>) -> Result<bool> {
    let Some(caller) = caller else { return Ok(false) };
    let owner: Option<Option<i64>> = conn
        .query_row(
            "SELECT owner FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?;
    let Some(Some(owner_id)) = owner else {
        return Ok(false); // no row, or the commons: nobody answers for it
    };
    let Some(caller_exact) = exact_pen_id(conn, caller)? else {
        return Ok(false);
    };
    // ★ OWNER ONLY, NOT ANCESTOR (Stet, review of #192). An ancestor that cannot read a row must not
    // re-classify it either: publishing a private row it cannot see is a quieter harm than deleting it,
    // not a gentler one. The owner answers for the row and may repair it.
    Ok(caller_exact == owner_id || owner_pen_id(conn, Some(caller))? == Some(owner_id))
}

pub(crate) fn may_write(conn: &Connection, key: &str, caller: Option<&str>) -> Result<()> {
    let row: Option<(Option<i64>, String)> = conn
        .query_row(
            "SELECT owner, audience FROM memory WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((owner, audience)) = row else {
        return Ok(()); // nothing there: a create, and creates are not gated here
    };
    let Some(caller) = caller else {
        // The commons (owner NULL) belongs to nobody and is writable by anyone, as it always was; a
        // row that HAS an owner needs a name before the question can even be asked.
        return if owner.is_none() {
            Ok(())
        } else {
            Err(Error::Invalid(format!(
                "{key} has an owner; a write must name the pen doing it"
            )))
        };
    };
    // ★ THE COMMONS IS DECIDED BY VISIBILITY ALONE, AND NEVER NEEDS A PEN ROW. An unattributed row
    // (`anonymous` author, owner NULL) is the one place where a name that resolves to nothing must not
    // turn a normal write into an error — the content is nobody's, so there is no authority to check.
    let Some(owner_id) = owner else {
        return if may_read_audience(conn, caller, &audience)? {
            Ok(())
        } else {
            Err(Error::Forbidden(format!(
                "{caller} may not write {key}: its audience does not include you"
            )))
        };
    };
    let caller_exact = exact_pen_id(conn, caller)?
        .ok_or_else(|| Error::Invalid(format!("no pen named {caller}")))?;

    // ★ THE OWNER MAY WRITE BLIND; AN ANCESTOR MAY NOT (Stet, review of #192, 2026-10-26). My first
    // version let ancestors through on a consistency argument — "it can already delete the row, so
    // refusing the gentler act is theatre". That argument is WRONG on measurement, and the measurement
    // decides it: a delete is LOUD and leaves a version, so the content survives and the act is
    // recorded; an overwrite is QUIET and plausible, so the row reads intact and wrong and the old
    // meaning survives only as history. **The gentler-sounding act is the one whose damage is invisible
    // when it happens**, which makes it the last thing to allow blind.
    //
    // The owner is the exception because the row is already attributed to it: telling it the truth
    // leaks nothing, and a row whose audience omits its own author has no other exit (#189).
    if caller_exact == owner_id || owner_pen_id(conn, Some(caller))? == Some(owner_id) {
        return Ok(());
    }
    // Everyone else must be able to SEE what they are writing…
    let visible = may_read_audience(conn, caller, &audience)?;
    if !visible {
        // ★ A WRITE REFUSAL SAYS FORBIDDEN, NOT ABSENT (James, 2026-10-24) — and it says WHICH reason
        // (#189). One sentence for three causes is how a correct refusal became a wrong diagnosis: this
        // used to tell the row's own author that the row "is not yours, and you are not an ancestor of
        // its owner", which is false twice.
        return Err(Error::Forbidden(format!(
            "{caller} may not write {key}: it exists, its audience does not include you, and you do not own it"
        )));
    }
    // …and must answer for it: an ancestor, or a pen holding a session that makes it one.
    if is_ancestor(conn, caller_exact, owner_id)? {
        return Ok(());
    }
    // ★ A HELD SESSION WRITES AS ITS POWERFUL PEN (2026-10-24): the whole point of borrowing the root
    // is reaching rows the tree does not put under you. `asserted_by` still names the hand that typed
    // it, so a borrowed write is attributable; the session row says whose authority it spent.
    for held in held_authority(conn, caller)? {
        if held == owner_id || is_ancestor(conn, held, owner_id)? {
            return Ok(());
        }
    }
    // ★ DELETE AND UPDATE DELIBERATELY CONSULT THIS SAME FUNCTION (Stet, review of #192). They used to
    // disagree — `delete` passed the requester, `update` passed the row's author — and that divergence
    // is what exposed the #190 bug. Two functions that must agree should BE one function; otherwise the
    // signal that found the bug becomes an accident nobody notices going quiet.

    let owner_name: String = conn.query_row(
        "SELECT name FROM pen WHERE id = ?1",
        params![owner_id],
        |r| r.get(0),
    )?;
    Err(Error::Forbidden(format!(
        "{caller} may not write {key}: it is owned by {owner_name}, and only that pen or one of its ancestors may"
    )))
}

fn add_impl(
    conn: &Connection,
    mem: &Memory,
    note: Option<&str>,
    expected: Option<i64>,
    actor: Option<&str>,
    credited: Option<&str>,
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
    // ★ AUTHORISE FIRST, THEN COMPARE (adversarial review of #192, 2026-10-26). The CAS used to run
    // BEFORE `may_write`, so a pen that may not write a row could call `upsert` with a wrong
    // `expected_version` and be told the row's CURRENT VERSION — existence and size — while every read
    // path correctly reported absence. The compare is still inside the transaction, where it belongs;
    // it is the ORDER that changed. A refusal must not be a read.
    // ★ NO FALLBACK TO THE CREDIT (soma/ellmephant#192, Stet's finding). `asserted_by` is the pen being
    // CREDITED; using it as the actor is #190 through another door. With no actor named, `may_write`
    // sees `None` — which FAILS CLOSED on an owned row and passes on an unowned one, where visibility
    // alone decides and there is no authority to steal.
    may_write(&tx, &prepared.key, actor)?;
    if let (Some(credited), Some(proven)) = (credited, actor) {
        if !credited.eq_ignore_ascii_case(proven) {
            may_act_as(&tx, proven, credited)?;
        }
    }
    if let Some(want) = expected {
        let current = live_version(&tx, &prepared.key)?;
        if current != want {
            return Ok((0, prepared.key, false, Some(current)));
        }
    }
    // ★ THE WRITE PATH CONSULTS OWNERSHIP (soma/ellmephant#109). Until now `owner` existed and
    // nothing asked it anything, so "descendants may write" was a sentence rather than a rule.
    //
    // ★ AND IT ASKS ABOUT THE **ACTOR**, NOT THE AUTHOR (soma/ellmephant#190, 2026-10-26). Authorship
    // is WRITE-ONCE (#107) on purpose — a patch carries the stored author through untouched so
    // re-filing a row cannot launder its provenance — but that made `mem.asserted_by` the *previous*
    // author on every update, so `may_write` was asking "may the row's own author write it?", which is
    // always yes. Measured consequence: a pen that could not even READ a row could overwrite it by
    // naming its key. The authority question and the provenance answer are two different questions and
    // must never be the same field. `actor` is who is writing; `mem.asserted_by` is who wrote it first.
    // ★ AND THE CREDIT MUST BE LEGITIMATE — BY LEND, NOT BY TREE SHAPE (Stet, review of #192, and #126).
    // My first attempt asked `may_write` about the credited pen too, which was the wrong question twice:
    // it tested AUTHORITY OVER THE ROW when the thing to test is AUTHORITY OVER THE NAME, and it would
    // have refused every legitimate `as:` (a parent crediting a child for work on the parent's own row,
    // which is what fingers are FOR). Ancestry is not the gate: every finger has an ancestor, so testing
    // ancestry here would undo #126 and make the refusal stop nothing.
    //
    // So: the actor half is "may you write this row?"; the credit half is "may you act as that name?"
    // — a descendant, not retired, with a LIVE lend. Two lower bounds, and neither can widen the other.
    if let (Some(credited), Some(proven)) = (credited, actor) {
        if !credited.eq_ignore_ascii_case(proven) {
            may_act_as(&tx, proven, credited)?;
        }
    }
    // ★ A ROW ITS OWN AUTHOR CANNOT READ IS A TRAP, AND IS REFUSED (soma/ellmephant#189 D1,
    // 2026-10-26). You may address a row to others; you may not address it to others ONLY. That
    // combination creates a row that exists, is stamped as yours, and is invisible to you — and before
    // #189's repair path it was unrecoverable: no read, no search, no backlink, no history, no write.
    // The store already refuses an audience naming a pen that does not exist, with the words "a row
    // nobody can see"; the author is a pen that does exist, and this is the same mistake.
    //
    // The check is on `asserted_by` — the AUTHOR, which is write-once — not on `actor`: an ancestor may
    // write a child's row, but the row still has to be readable by the pen it is attributed to. `public`
    // and `subtree:` audiences that contain the author pass, which is why no escape hatch is needed.
    // An EMPTY audience is the legacy spelling of `public` (the column defaults to it on read), so it
    // is included rather than sent through the address parser.
    //
    // ★ NO CARVE-OUT FOR NARROWING TO AN ANCESTOR — DECIDED, 2026-10-26. It is tempting: a hand making
    // a note private to its parent can still RECOVER it, because the owner may write blind (#192), so
    // the row is not permanently lost. But recoverability does not undo the harm: the author still
    // cannot read words it wrote, which is exactly what #189 measured as a lost day. The remedies are
    // one token (`pen:<you>`) or one honest credit (`as: <parent>`, with a live lend), and both are
    // named in the refusal. Every exception here would also need a subset relation between audiences
    // (is `subtree:x` narrower than `pen:y`?), and the strength of this rule is that it has no such
    // relation to get wrong — the two-step bypass test exists because that is where the weight sits.
    let addressed = !mem.audience.trim().is_empty() && mem.audience != audience::PUBLIC;
    if addressed {
        // ★ THE NAMED PENS MUST EXIST BEFORE WE ASK WHETHER THE AUTHOR IS AMONG THEM. `validated` also
        // runs later, in `apply`; running it here keeps the FIRST refusal the more fundamental one —
        // "`nosuchpen` names no pen" beats "it would be invisible to you", because a typo is a typo.
        audience::validated(&tx, &mem.audience)?;
    }
    // ★ AN AUTHOR WHO IS NOBODY IS NOT PROTECTED BY THIS RULE (second adversarial pass, 2026-10-26).
    // #122's backfill left rows with `asserted_by = anonymous`. There is no author to hide his own
    // words from, so the rule has nothing to enforce — and enforcing it anyway made those rows
    // unwritable forever, with the remedy "add `pen:anonymous`", which is an address to a pen that
    // does not exist. A rule that cannot be satisfied by any legal write is a trap of its own.
    // ★ THE EXEMPTION IS FOR ROWS THAT WERE ALREADY UNATTRIBUTED, NOT FOR CALLERS WHO CLAIM IT
    // (hostile review, round 1). Applied to any write whose author string was `anonymous`, it was a
    // bypass with a legacy alibi: name yourself anonymous, address the row to someone else, and the
    // rule that exists to prevent exactly that never runs. So it is keyed on the STORED row.
    let stored_audience: Option<String> = tx
        .query_row(
            "SELECT audience FROM memory WHERE key = ?1",
            params![prepared.key],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let stored_author: Option<String> = tx
        .query_row(
            "SELECT asserted_by FROM memory WHERE key = ?1",
            params![prepared.key],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let is_anonymous = |a: &str| a.trim().is_empty() || a.eq_ignore_ascii_case(crate::identity::ANONYMOUS);
    let legacy_unattributed = stored_author.as_deref().is_some_and(is_anonymous);
    let author = mem.asserted_by.as_deref().map(str::trim).filter(|a| !a.is_empty());
    if addressed && author.is_some_and(is_anonymous) && !legacy_unattributed {
        return Err(Error::Invalid(format!(
            "`{}` would be addressed to others only while its author claims to be unattributed \
             (`{}`). An unattributed row has nobody to read it: name the pen that writes it, or pass \
             `as: <pen>` with a live lend so the row is theirs.",
            prepared.key,
            mem.asserted_by.as_deref().unwrap_or("")
        )));
    }
    let author = author.filter(|a| !is_anonymous(a) || legacy_unattributed);
    if let (true, Some(author)) = (addressed, author) {
        if !is_anonymous(author) && !may_read_audience(&tx, author, &mem.audience)? {
            // ★ SAY "IS", NOT "WOULD BE" (hostile review, round 4). For a row that is ALREADY trapped,
            // "this would be invisible" invites the caller to look for what their write introduced —
            // and their write introduced nothing; the row has been unreadable since before the call.
            // A message that names the wrong moment names the wrong cause, which is the whole class
            // this PR exists to remove.
            let was_trapped = stored_audience
                .as_deref()
                .map(|a| !a.trim().is_empty() && a != audience::PUBLIC)
                .unwrap_or(false);
            return Err(Error::Invalid(format!(
                "`{}` {} invisible to its own author: the audience is `{}` and does not include \
                 `{author}`. {}\n\nAdd `pen:{author}` — or `subtree:<one of {author}'s ancestors>` if \
                 it is for a branch — or make it `public`. If this is genuinely a note FOR someone else \
                 and not for you, credit them (`as: <pen>`, which needs a live lend) so the row is \
                 theirs and the rule has an author who can read it. A row may be addressed to others; \
                 it may not be addressed to others ONLY, because then nobody who answers for it can \
                 read it.",
                prepared.key,
                if was_trapped { "IS ALREADY" } else { "would be" },
                mem.audience,
                if was_trapped {
                    "This write did not create that state — the row was already unreadable to the pen \
                     that wrote it, and this write does not change the audience."
                } else {
                    ""
                }
            )));
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
    ///
    /// ★ THIS IS THE **ATTRIBUTED** PEN, NOT NECESSARILY THE ACTOR (soma/ellmephant#192). With `as:`
    /// it names the descendant that takes the credit; the pen that actually sent the request is
    /// [`Patch::acted_by`]. Authority requires BOTH to be permitted (Stet, review of #192): crediting a
    /// child must not launder an authority its parent lacks, and a parent must not be refused a row it
    /// may write because it chose to give a child the credit.
    pub asserted_by: Option<String>,
    /// The pen that actually made the request, as the connection proved it. `None` means "same as
    /// `asserted_by`" — the ordinary case, where nobody is being credited.
    pub acted_by: Option<String>,
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
    /// An explicit audience: `public`, or addresses (`pen:<name>` exact, `subtree:<name>` inclusive).
    /// Unlike `private`, this NAMES others, so it is validated against the registry at write time.
    pub audience: Option<String>,
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
fn guard_protected(
    conn: &Connection,
    key: &str,
    was: &[String],
    patch: &Patch,
    writer: Option<&str>,
) -> Result<()> {
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
    // ★ THE HOLDER IS THE PEN THAT ACTS, NOT THE NAME IT CREDITS (third adversarial pass, 2026-10-26).
    // This read `patch.asserted_by` — the credited pen — so a hand holding the lease and crediting its
    // finger for the work was refused its own protection. The lease is taken by the pen that acts; the
    // credit is a name. Same asymmetry as `guard_audience`, found by looking for it rather than waiting
    // for it to bite.
    if let Some(writer) = writer.filter(|w| !claim.owner.eq_ignore_ascii_case(w)) {
        return Err(Error::Invalid(format!(
            "`{key}` is PROTECTED and leased by `{}` until {}; `{writer}` does not hold it.",
            claim.owner, claim.expires_at
        )));
    }
    Ok(())
}

/// A delete has no patch to inspect, so it only asks: is a live lease held on this key?
fn guard_protected_delete(conn: &Connection, key: &str, writer: Option<&str>) -> Result<()> {
    let Some(mem) = get(conn, key)? else {
        return Ok(());
    };
    if !is_protected(&mem.tags) {
        return Ok(());
    }
    match crate::store::claim::get(conn, key)? {
        // ★ THE LEASE MUST BE THE DELETER'S (third adversarial pass, 2026-10-26). This accepted ANY live
        // lease, so a pen with authority over the row could delete a protected memory while ANOTHER pen
        // held the lease — "someone is editing this" was read as "you may remove it". The write path
        // (`guard_protected`) has always asked whose lease it is; the delete path did not. Protection
        // that stops an edit and not a deletion is a lock on the window.
        Some(claim) if claim.live => match writer {
            Some(w) if claim.owner.eq_ignore_ascii_case(w) => Ok(()),
            Some(w) => Err(Error::Invalid(format!(
                "`{key}` is PROTECTED and leased by `{}` until {}; `{w}` does not hold it. A live lease \
                 held by someone else is not permission to delete.",
                claim.owner, claim.expires_at
            ))),
            // An unproven deleter cannot be told apart from any other client; the lease is the
            // strongest statement available and it was taken on purpose (as on the write path).
            None => Ok(()),
        },
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
fn guard_audience(
    conn: &Connection,
    existing: Option<&Memory>,
    patch: &Patch,
    writer: Option<&str>,
) -> Result<()> {
    let Some(old) = existing else {
        return Ok(());
    };
    if old.audience == audience::PUBLIC {
        return Ok(());
    }
    // ★ BOTH FORMS OF AUDIENCE CHANGE ARE GUARDED (adversarial review, 2026-10-24). This checked
    // only `private`; the explicit `audience` argument I just added would have walked straight past
    // it, letting any pen re-classify a row it cannot read. The boundary is the boundary whichever
    // door the change arrives at.
    if patch.private.is_none() && patch.audience.is_none() {
        return Ok(());
    }
    // ★ BUT THE PENS THAT ANSWER FOR THE ROW ARE NOT STRANGERS (soma/ellmephant#189, 2026-10-26).
    // Without this, a row whose audience omits its own author had NO exit: the author could not read
    // it, could not write it, could not re-classify it, and could only watch an ancestor delete it.
    // The guard exists to stop a pen that cannot read a row from publishing it — an owner or an
    // ancestor already has that power through deletion, so refusing them the gentler act is theatre.
    if answers_for(conn, &old.key, writer)? {
        return Ok(());
    }
    // The pen a row is ADDRESSED to may re-classify it — that pen owns the boundary.
    let writer = writer.unwrap_or_default();
    let addressed_exactly = audience::parse(&old.audience)
        .is_some_and(|targets| targets.iter().any(|t| t.token() == audience::pen(writer)));
    if addressed_exactly {
        return Ok(());
    }
    // Re-asserting the identical audience is a no-op, not a change.
    let unchanged = patch
        .audience
        .as_deref()
        .and_then(audience::parse)
        .is_some_and(|t| {
            let mut a: Vec<String> = t.iter().map(|x| x.token()).collect();
            let mut b: Vec<String> = audience::parse(&old.audience)
                .unwrap_or_default()
                .iter()
                .map(|x| x.token())
                .collect();
            a.sort();
            b.sort();
            a == b
        });
    if unchanged {
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
    // ★ WHO IS WRITING, ONCE (adversarial review of #192). `acted_by` is the proven pen; `asserted_by`
    // is the credited one. Authority, the boundary guard and the write all have to agree about which
    // pen is acting, or a hand crediting its finger gets judged as the finger for some checks and as
    // the hand for others.
    // ★ THE ACTOR, NEVER THE CREDIT (soma/ellmephant#192): the guards below must judge the pen that
    // acts. Falling back to `asserted_by` is how a write gets authorised as the name it credits.
    // ★ `private` AND `as:` CONTRADICT EACH OTHER (hostile review of #195, round 3). `private: true`
    // addresses the row to the pen that ACTS; `as:` credits a different pen. Both cannot be true, and
    // the failure was ugly rather than refused: with the audience on the actor, the CREDITED pen could
    // no longer read words it was credited for, so the D1 rule refused the write with a message about
    // invisibility — a true sentence that names the wrong cause. Refuse the combination instead.
    if patch.private == Some(true) {
        if let (Some(credited), Some(actor)) = (patch.asserted_by.as_deref(), patch.acted_by.as_deref())
        {
            if !credited.eq_ignore_ascii_case(actor) {
                return Err(Error::Invalid(
                    "`private: true` addresses the row to the pen that ACTS, and `as:` credits a \
                     different pen — they contradict each other. Pass an explicit `audience` (e.g. \
                     `pen:<you>,pen:<them>`) if the row is for both, or drop one of them."
                        .into(),
                ));
            }
        }
    }
    let writer = patch.acted_by.as_deref();
    // ★ AUTHORISE BEFORE THE GUARDS (adversarial review of #192). `guard_protected` and
    // `guard_audience` produce different sentences for different states, so running them first let a
    // pen that may not write the row learn that it exists and is not public — a read disguised as a
    // refusal. A refusal must not be a read; the same rule as the CAS ordering.
    if existing.is_some() {
        may_write(conn, &key, writer)?;
    }
    guard_protected(conn, &key, &was_tags, patch, writer)?;
    // An audience belongs to the pen that set it: a caller may not publish, or privatise, a row it
    // could not read. Without this, `private` would be a lock whose key hangs beside the door.
    guard_audience(conn, existing.as_ref(), patch, writer)?;

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
                    // ★ THE WRITER, NOT THE CREDIT (Stet, review of #192): a parent writing as its
                    // child must be recorded as the parent, or the laundering survives in the audit trail
                    // after being blocked at the gate.
                    // ★ AN UNKNOWN WRITER IS HONEST; A CREDITED WRITER IS A GUESS (Stet's review of
                    // #192). Falling back to `asserted_by` here would record the pen being CREDITED as
                    // the pen that wrote — the exact defect the `edited_by`-from-`acted_by` fix exists
                    // to prevent, surviving in the one branch that kept a fallback.
                    edited_by: patch.acted_by.clone().or_else(|| old.edited_by.clone()),
                    origin: patch.origin.clone().or_else(|| old.origin.clone()),
                    audience: match (&patch.audience, patch.private) {
                        // An explicit audience is a SHARE (it names others), so it is checked against
                        // the registry; `private` is derived from the proven pen and cannot.
                        (Some(spec), _) => audience::validated(conn, spec)?,
                        // ★ `private` NAMES THE PEN THAT ACTS, NOT THE PEN IT CREDITS (hostile review of
                        // #195, round 2). This read `asserted_by` — the credit — so a hand crediting its
                        // finger made the row private to the FINGER, and a legacy row with an
                        // `anonymous` author could be made private to a pen that does not exist. The
                        // proven pen is the one making the row private; the credit is a name.
                        (None, Some(p)) => audience::for_write(
                            p,
                            patch
                                .acted_by
                                .as_deref()
                                .or(patch.asserted_by.as_deref())
                                .or(old.asserted_by.as_deref()),
                        )?,
                        (None, None) => old.audience.clone(),
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
                    edited_by: patch.acted_by.clone().or_else(|| patch.asserted_by.clone()),
                    origin: patch.origin.clone().filter(|s| !s.trim().is_empty()),
                    audience: match &patch.audience {
                        Some(spec) => audience::validated(conn, spec)?,
                        None => audience::for_write(
                            patch.private == Some(true),
                            // ★ The same correction on the CREATE path: the pen making the row private
                            // is the one that acts; the credit is a name and may be a finger.
                            patch
                                .acted_by
                                .as_deref()
                                .or(patch.asserted_by.as_deref()),
                        )?,
                    },
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
        // ★ WHO IS WRITING, which is not who the row is attributed to: on an update `mem.asserted_by`
        // is the stored author (write-once, #107), so passing it here would authorise every update as
        // the row's own author (#190). `acted_by` is the proven pen; `asserted_by` is the credited one,
        // and authority needs both.
        // ★ THE ACTOR, AND NOT THE CREDIT (soma/ellmephant#192): an absent actor must fail closed, not
        // be filled in from the name being credited.
        patch.acted_by.as_deref(),
        patch.asserted_by.as_deref(),
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
    // ★ ONE PLACE FOR THE BOUNDARY (adversarial review, 2026-10-24): `merge` validated a caller's
    // `audience`, but `add` (tests, imports, the CLI) took whatever `Memory` carried. An audience that
    // names no pen is a row NOBODY can read, and failing silently CLOSED is the worst direction. Every
    // write goes through here.
    let checked_audience = audience::validated(tx, audience::effective(&mem.audience))?;
    let id = match prepared.existing {
        Some((id, _, _)) => {
            if pending {
                // Snapshot what this write is about to replace, in the SAME transaction: a row and
                // its history must not be able to disagree (migration 0011). Only a DOCUMENT change
                // earns a version, which is exactly what `pending` means — a tag-only write is
                // filing, not meaning.
                version::snapshot(tx, id, &prepared.key, prepared.note.as_deref())?;
            }
            update_row(tx, id, mem, &checked_audience)?;
            id
        }
        None => insert_row(tx, &prepared.key, mem, &checked_audience)?,
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

/// The exact row for a name — **no parent resolution**, which is the only thing "exact" means here.
///
/// ★ IT IS CASE-INSENSITIVE, AND THE NAME SUGGESTS OTHERWISE (Stet, review of #193, 2026-10-26).
/// `lower(name) = lower(?1)` — so `exact_pen_id("James")` and `exact_pen_id("james")` are one row.
/// The adjective distinguishes it from [`owner_pen_id`], which resolves a FINGER to its parent; it has
/// never meant "exact case", and a reader who assumed it did would conclude every fold in the tree was
/// unsafe. Uniqueness of `lower(name)` is enforced by the triggers in migration `0027` (and by
/// `store::ensure_name_uniqueness` once the table is clean), so a folded lookup cannot pick between two
/// rows — which is the fact this comment exists to make checkable rather than assumed.
fn exact_pen_id(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| r.get(0),
        )
        .optional()?)
}

/// Is `ancestor` anywhere above `of` in the tree? Walks the parent chain, bounded: a cycle is a bug,
/// not a shape, and it must not hang a request.
pub(crate) fn is_ancestor(conn: &Connection, ancestor: i64, of: i64) -> Result<bool> {
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

/// Make sure a name has a pen row, and return the id it OWNS as. The identity table is the only
/// authenticator, so a name that can authenticate IS a pen; without this, a self that had never
/// written would have its first row land in the commons (owner NULL) — absence-with-a-meaning it
/// never chose (#109, review round 4).
fn ensure_pen(conn: &Connection, name: &str) -> Result<i64> {
    if let Some(id) = owner_pen_id(conn, Some(name))? {
        return Ok(id);
    }
    conn.execute(
        "INSERT OR IGNORE INTO pen (name, kind) VALUES (?1, 'self')",
        params![name],
    )?;
    owner_pen_id(conn, Some(name))?
        .ok_or_else(|| Error::Invalid(format!("could not register pen {name}")))
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
               FROM pen author WHERE lower(author.name) = lower(?1)",
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
fn update_row(tx: &Connection, id: i64, mem: &Memory, audience: &str) -> Result<()> {
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
            audience,
            mem.edited_by
        ],
    )?;
    Ok(())
}

/// The `INSERT` half of [`add`]: a new key; returns its rowid.
///
/// `encode_hash` starts at [`ENCODE_PENDING`] — a fresh row always owes an encode — and the worker
/// settles it with the real hash of the vector it writes.
fn insert_row(tx: &Connection, key: &str, mem: &Memory, audience: &str) -> Result<i64> {
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
            audience,
            mem.edited_by,
            match mem.asserted_by.as_deref() {
                Some(name) => Some(ensure_pen(tx, name)?),
                None => None,
            }
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Take responsibility for a row. `owner` moves to the adopting pen; a finger's parent is what
/// actually owns, so adopting as `stet/finger-3` is adopting as `stet` (#109).
pub fn adopt(conn: &Connection, key: &str, pen: &str, force: bool) -> Result<()> {
    assign(conn, key, pen, pen, force)
}

/// Give `key` to `to`, acting as `actor`.
///
/// Three rules, all the store's: the actor must be able to READ the row (a private row cannot be
/// assigned by somebody who cannot see it); a pen may name itself or a pen in its own subtree; and
/// naming anybody else needs a LIVE borrow of `estate`, because root reach is lent, never assumed.
/// An owned row is taken from its owner only by an ancestor of that owner (or with root reach), and
/// only with `force`.
pub fn assign(conn: &Connection, key: &str, actor: &str, to: &str, force: bool) -> Result<()> {
    let audience: Option<String> = conn
        .query_row(
            "SELECT audience FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?;
    let audience = audience.ok_or_else(|| Error::NotFound(key.to_string()))?;
    if !may_read_audience(conn, actor, &audience)? {
        return Err(Error::Invalid(format!(
            "{key} is private to {audience}; a pen cannot assign a row it cannot read"
        )));
    }
    let to_id = owner_pen_id(conn, Some(to))?.ok_or_else(|| {
        Error::Invalid(format!(
            "no pen named {to}; the pen registry has no such row"
        ))
    })?;
    let actor_id = exact_pen_id(conn, actor)?
        .ok_or_else(|| Error::Invalid(format!("no pen named {actor}")))?;
    // A finger's parent is what OWNS, so "assign to myself" must compare owning ids, not names.
    let actor_owns = owner_pen_id(conn, Some(actor))?
        .ok_or_else(|| Error::Invalid(format!("no pen named {actor}")))?;
    // ★ ROOT REACH IS A LIVE BORROW, NEVER A NAME (2026-10-24). `estate` has no identity, so the
    // only way to hold its reach is a session that expires and can be revoked.
    let root = crate::store::sudo::live(conn, actor, "estate")?.is_some();
    if actor_owns != to_id && !root && !is_ancestor(conn, actor_id, to_id)? {
        return Err(Error::Invalid(format!(
            "{actor} may assign only to itself or its own subtree; assigning to {to} needs a live \
             borrow of estate"
        )));
    }
    // An OWNED row may be taken only by an ancestor, and only deliberately (#109, James).
    let from: Option<i64> = conn
        .query_row(
            "SELECT owner FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if let Some(owner_id) = from {
        if owner_id == to_id {
            return Ok(()); // already theirs; nothing to do
        }
        if !root && !is_ancestor(conn, actor_id, owner_id)? {
            return Err(Error::Invalid(format!(
                "{actor} may not reassign {key}: only an ancestor of its owner may take an owned row"
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
        params![key, to_id],
    )?;
    if n == 0 {
        return Err(Error::NotFound(key.to_string()));
    }
    // `pen_event` accepts a fixed vocabulary; `transfer` is the one that means "the holder moved".
    let action = if actor_owns == to_id {
        "adopt"
    } else {
        "transfer"
    };
    log_pen_event(conn, key, action, Some(actor_id), from, Some(to_id), None)
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
    // ★ AND PUBLISHING WHAT YOU CANNOT READ IS WORSE (2026-10-24). With `pen:` now exact, an ancestor
    // may reach a descendant's row without being able to see it. Publishing words you have never read
    // is not oversight, it is a leak with extra steps.
    if was_private {
        let audience: String = conn.query_row(
            "SELECT audience FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )?;
        if !may_read_audience(conn, pen, &audience)? {
            return Err(Error::Invalid(format!(
                "{pen} cannot publish {key}: the row is addressed in a way that does not include \
                 {pen}, so releasing it would publish words it cannot read"
            )));
        }
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
/// One row of the responsibility ledger: `(at, action, from_pen, to_pen, actor)`.
///
/// `from`/`to` are the row's holders before and after; `actor` is the pen that did it — which is
/// not always either of them, because an ancestor may act on a descendant's row, and a DELETE has
/// no `to` at all. Appended last so the older readers of this tuple keep their positions.
pub type PenEvent = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// What a retirement would do, computed without doing it.
#[derive(Debug, Clone, Serialize)]
pub struct RetirePreview {
    pub pen: String,
    pub parent: Option<String>,
    pub rows: i64,
    pub private_rows: i64,
}

/// What one pen IS, and what it is to the pen asking — the answer to "who is this, and what are
/// they to me" (James, 2026-10-23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Whois {
    /// The pen's name: an address, never a permission.
    pub name: String,
    /// `self` or `finger`.
    pub kind: String,
    /// The pen that minted it. `None` only for a root.
    pub parent: Option<String>,
    /// When the row was created.
    pub created: String,
    /// The tombstone, when it has ceased. A retired pen still answers: history outlives it.
    pub ceased: Option<String>,
    /// The walk from the ASKING pen to this one, `ME`, or `None` when they share no root.
    pub relation: Option<String>,
    /// The rows this pen answers for.
    pub owns: i64,
    /// How many pens hang BELOW this one. A pen asking `whois <itself>` learns how far it reaches —
    /// the one number that says what ancestor read actually grants it, rather than leaving a reader to
    /// infer it from a tree it cannot see (#120).
    pub descendants: i64,
}

/// A pen row as `whois` reads it: `(id, kind, parent_id, created, ceased_at)`.
// id, the row's own spelling of the name, kind, parent, created, ceased (#191: the spelling is
// part of the answer, so it is part of the row we read).
type PenFacts = (i64, String, String, Option<i64>, String, Option<String>);

/// Look up one pen as `viewer` sees it.
///
/// The relationship is the point: a name alone is an address, and "what are they to me" is the
/// question that decides whether an act is allowed. `None` for the walk means the two share no root,
/// which is a real answer and not an error — a pen from another estate is a pen nobody here can
/// reach.
pub fn whois(conn: &Connection, viewer: &str, name: &str) -> Result<Whois> {
    let row: Option<PenFacts> = conn
        .query_row(
            // ★ THE ROW'S SPELLING, NOT THE QUERY'S (soma/ellmephant#191). `name` in the reply used to
            // be the string the caller typed, so asking for `JAMES` answered `"name": "JAMES"` — a field
            // called `name` echoing the question instead of stating the row. Case-PRESERVING means the
            // stored spelling is what gets shown; case-INSENSITIVE means any spelling may be asked for.
            "SELECT id, name, kind, parent_id, created, ceased_at FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((id, stored_name, kind, parent_id, created, ceased)) = row else {
        return Err(Error::Invalid(format!("no pen named {name}")));
    };
    let parent = match parent_id {
        Some(pid) => Some(conn.query_row(
            "SELECT name FROM pen WHERE id = ?1",
            params![pid],
            |r| r.get(0),
        )?),
        None => None,
    };
    let owns: i64 = conn.query_row(
        "SELECT count(*) FROM memory WHERE owner = ?1",
        params![id],
        |r| r.get(0),
    )?;
    let descendants: i64 = conn.query_row(
        "WITH RECURSIVE sub(id) AS (
             SELECT id FROM pen WHERE parent_id = ?1
             UNION
             SELECT c.id FROM pen c JOIN sub s ON c.parent_id = s.id
         )
         SELECT count(*) FROM sub",
        params![id],
        |r| r.get(0),
    )?;
    Ok(Whois {
        name: stored_name.clone(),
        kind,
        parent,
        created,
        ceased,
        relation: relation::relation(conn, viewer, &stored_name)?.map(|k| k.gloss()),
        owns,
        descendants,
    })
}

fn pen_row_by_id(conn: &Connection, id: i64) -> Result<(i64, Option<i64>, Option<String>)> {
    conn.query_row(
        "SELECT id, parent_id, ceased_at FROM pen WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen with id {id}")))
}

fn pen_row(conn: &Connection, name: &str) -> Result<(i64, Option<i64>, Option<String>)> {
    conn.query_row(
        "SELECT id, parent_id, ceased_at FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

/// ★ ATTACH THE TREE FROM CONFIGURATION (2026-10-24). The seed gives NAMES, provisioning gives
/// STANDING — and until now "provisioning" meant "somebody runs SQL", which nothing could do: the image
/// has no sqlite3, the PVC cannot be mounted beside a running brain, and no API sets a parent. A store
/// would roll with five roots, report itself ready, and every ancestry rule would reach nothing.
///
/// So the attach is configuration, in the same shape as `ELLMEPHANT_SUDOERS`:
///
/// ```text
/// ELLMEPHANT_PEN_TREE="james=estate,stet=james,cline=james"
/// ```
///
/// Read at open, idempotent, and NEVER FATAL — a typo costs one refused edge with a named reason, not
/// an outage.
pub const TREE_ENV: &str = "ELLMEPHANT_PEN_TREE";

/// A pen's id by NAME, folding case — because everything else does.
///
/// The identity table matches claims case-insensitively and `audience::pen()` lowercases for the
/// audience, so `James` and `james` are ONE pen to every layer above this one. A case-sensitive lookup
/// is how a config line (`james=estate`) silently attaches nothing while the store's own pen is spelled
/// `James` — found by rehearsing the roll, not by reading the code. Since migration 0027 the registry
/// cannot hold two names that differ only in case, so this can no longer be ambiguous.
fn pen_id_named(conn: &Connection, name: &str) -> Result<i64> {
    Ok(pen_row(conn, name)?.0)
}

/// ★ THE INDEX IS THE FAST PATH, NOT THE GUARANTEE (migration 0027 holds the triggers). Created here,
/// guarded, because a unique index REFUSES to be built over a store that already holds a case-collision
/// — and a store that will not open is worse than one with a warning in it. When the table is clean this
/// is one statement; when it is not, the collision is NAMED and left for a human, and the triggers still
/// refuse every new write.
pub fn ensure_name_uniqueness(conn: &Connection) -> Result<()> {
    let collisions: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT group_concat(name, ' / ') FROM pen GROUP BY lower(name) HAVING count(*) > 1",
        )?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    if collisions.is_empty() {
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS pen_name_nocase ON pen(lower(name))",
            [],
        )?;
    } else {
        tracing::warn!(
            collisions = %collisions.join("; "),
            "pen names collide regardless of case: new writes are refused, and a human must decide which row answers"
        );
    }
    Ok(())
}

/// Point one pen at a parent, refusing the mistakes that would make the tree a lie.
///
/// The DELIBERATE act: it overwrites whatever the table says. The seed ([`seed_parent`]) only fills a
/// blank; a human or a verb that means it calls this.
pub fn set_parent(conn: &Connection, child: &str, parent: &str) -> Result<()> {
    let child_id = pen_id_named(conn, child)?;
    let (_, child_parent, child_ceased) = pen_row_by_id(conn, child_id)?;
    if child_ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{child} has been retired: a tombstone is not re-parented"
        )));
    }
    let parent_id = pen_id_named(conn, parent)?;
    let (_, _, parent_ceased) = pen_row_by_id(conn, parent_id)?;
    if parent_ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{parent} has been retired: a ceased pen cannot be a parent"
        )));
    }
    if child_id == parent_id {
        return Err(Error::Invalid(format!("{child} cannot be its own parent")));
    }
    // A cycle would make `is_ancestor` walk forever and the root unnameable.
    if is_ancestor(conn, child_id, parent_id)? {
        return Err(Error::Invalid(format!(
            "{parent} is below {child}: that edge would make the tree a loop"
        )));
    }
    if child_parent == Some(parent_id) {
        return Ok(()); // already attached: idempotent, and not worth a write
    }
    conn.execute(
        "UPDATE pen SET parent_id = ?2 WHERE id = ?1",
        params![child_id, parent_id],
    )?;
    Ok(())
}

/// What a SEED found: attached a bare pen, agreed with the table, or disagreed and stood down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seeded {
    Attached,
    Already,
    /// The table says `Some(current)`; the seed says something else. **The table wins.**
    Disagrees(String),
}

/// ★ A SEED IS NOT AN AUTHORITY (James, 2026-10-24: *"the estate is in an environment variable instead of
/// a table?"*). It is not, and it must not be. The env var exists only because nothing could run the
/// attach at all; it must not become the place where the estate LIVES, or a deliberate re-parenting, an
/// adopt, or a disclaim would be silently undone by the next restart — and a restart is exactly when
/// nobody is watching.
///
/// So: seed only a pen that has **no parent yet**. Once the table has an opinion, the table wins forever,
/// and a disagreeing seed is reported as drift rather than obeyed. `set_parent` remains the deliberate,
/// overwriting act — the one you reach for on purpose.
pub fn seed_parent(conn: &Connection, child: &str, parent: &str) -> Result<Seeded> {
    let child_id = pen_id_named(conn, child)?;
    let (_, current, ceased) = pen_row_by_id(conn, child_id)?;
    if current.is_some() {
        let current_name: String = conn.query_row(
            "SELECT name FROM pen WHERE id = ?1",
            params![current],
            |r| r.get(0),
        )?;
        let agrees = pen_id_named(conn, parent)
            .map(|id| Some(id) == current)
            .unwrap_or(false);
        return Ok(if agrees {
            Seeded::Already
        } else {
            Seeded::Disagrees(current_name)
        });
    }
    if ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{child} has been retired: a tombstone is not re-parented"
        )));
    }
    set_parent(conn, child, parent)?;
    Ok(Seeded::Attached)
}

/// Apply [`TREE_ENV`] as a SEED. Returns `(attached, already, disagreed, rejected)`.
///
/// Disagreements are logged **by name and both sides** — configuration drift is worth seeing, never worth
/// obeying.
pub fn load_tree_at_boot(conn: &Connection) -> (usize, usize, usize, usize) {
    let Ok(spec) = std::env::var(TREE_ENV) else {
        return (0, 0, 0, 0);
    };
    let (mut attached, mut already, mut disagreed, mut rejected) = (0, 0, 0, 0);
    for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let Some((child, parent)) = entry.split_once('=') else {
            tracing::warn!(spec = %entry, "pen tree: expected `child=parent`");
            rejected += 1;
            continue;
        };
        match seed_parent(conn, child.trim(), parent.trim()) {
            Ok(Seeded::Attached) => attached += 1,
            Ok(Seeded::Already) => already += 1,
            Ok(Seeded::Disagrees(current)) => {
                tracing::warn!(
                    child = %child.trim(),
                    seed = %parent.trim(),
                    table = %current,
                    "pen tree: seed disagrees with the TABLE — the table wins; change it on purpose with `set_parent`"
                );
                disagreed += 1;
            }
            Err(e) => {
                tracing::warn!(spec = %entry, error = %e, "pen tree: edge rejected, the rest applied");
                rejected += 1;
            }
        }
    }
    tracing::info!(
        attached,
        already,
        disagreed,
        rejected,
        "pen tree: {attached} seeded, {already} already attached, {disagreed} refused in favour of the table, {rejected} malformed"
    );
    (attached, already, disagreed, rejected)
}

/// A pen created at runtime: whoever minted it answered for it, and it answered to them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Minted {
    /// The new pen's name.
    pub name: String,
    /// `self` or `finger` — what KIND of thing it is, never inferred from the name.
    pub kind: String,
    /// The pen that minted it: the parent, by right, never by argument.
    pub parent: String,
}

/// **MINT: a pen creates a pen** (James, 2026-10-23). The runtime door.
///
/// The parent is the CALLER — you may create downward, never sideways and never upward, because
/// parentage is the grant and a name is an address rather than a permission. A hand mints a finger;
/// `estate`'s children are minted by provisioning, which is the other door and a different act.
///
/// ★ A NAME IS SPENT. A retired pen keeps its row (a tombstone, `ceased_at`) and its name stays
/// taken, so this refuses rather than resurrecting. Two lives under one name would make every ledger
/// row ambiguous — "who did this" cannot mean two different pens — and the whole point of the
/// tombstone is that history outlives the thing.
pub fn mint(conn: &Connection, caller: &str, name: &str, kind: &str) -> Result<Minted> {
    if kind != "self" && kind != "finger" {
        return Err(Error::Invalid(format!(
            "kind must be `self` or `finger`, not `{kind}`"
        )));
    }
    let (caller_id, _, ceased) = pen_row(conn, caller)?;
    if ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{caller} has been retired: a pen that has ceased cannot create pens"
        )));
    }
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Invalid("a pen needs a name".into()));
    }
    // ★ THE RESERVED WORD IS NOT A NAME (found reviewing #123). `anonymous` means "no currently
    // existing pen wrote this", so minting a pen by that name would make every unattributed count
    // ambiguous again — the defect 0021 and 0025 exist to remove.
    if name.eq_ignore_ascii_case(crate::identity::ANONYMOUS) {
        return Err(Error::Invalid(format!(
            "`{name}` is reserved: it means NO currently existing pen, so it can never be one"
        )));
    }
    // ★ AND A NAME MUST BE ADDRESSABLE. A private row's audience is `pen:<name>`, and an audience
    // containing a comma is refused on write — so a pen whose name holds one could never hold a
    // private row, and nobody would learn that until the first private write failed.
    // ★ CASE-INSENSITIVE, CASE-PRESERVING (James, 2026-10-24; migration 0027). The trigger enforces it,
    // but a constraint abort names the index, not the pen — and the useful sentence is "that name is
    // already taken, spelled like this".
    if let Some((existing, ceased)) = conn
        .query_row(
            "SELECT name, ceased_at FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?
    {
        // A RETIRED pen's name has its own, better refusal ("spent") further down; this one is about the
        // live collision the schema cannot see.
        if ceased.is_some() {
            return Err(Error::Invalid(format!(
                "`{name}` is already a pen, spelled `{existing}`, and it has been retired: a name is \
                 spent — two lives under one name make every ledger row ambiguous"
            )));
        }
        return Err(Error::Invalid(format!(
            "`{name}` is already a pen, spelled `{existing}`: names are unique regardless of case, and the \
             spelling is preserved rather than folded"
        )));
    }
    if name.contains(',') || name.chars().any(char::is_whitespace) {
        return Err(Error::Invalid(format!(
            "{name} cannot be a pen name: audiences are `pen:<name>`, and a comma or a space cannot be \
             addressed"
        )));
    }
    if let Some((_, _, ceased_at)) = conn
        .query_row(
            "SELECT id, parent_id, ceased_at FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?
    {
        return Err(Error::Invalid(match ceased_at {
            Some(at) => format!(
                "{name} was a pen retired at {at}: a name is spent, and reusing it would make every \
                 ledger row mean two things"
            ),
            None => format!("{name} is already a pen"),
        }));
    }
    conn.execute(
        "INSERT INTO pen (name, kind, parent_id) VALUES (?1, ?2, ?3)",
        params![name, kind, caller_id],
    )?;
    Ok(Minted {
        name: name.to_string(),
        kind: kind.to_string(),
        parent: caller.to_string(),
    })
}

/// May `proven` be credited as `claimed` on a write? Itself, or anything below it ON A LIVE LEND.
///
/// A finger holds no credential of its own — it is how one self does more work, not a party — so its
/// work is written under the hand's PROOF and the finger's NAME. This is the check that keeps that
/// honest: a hand may credit its own subtree, and nothing else. Without it the write path would take
/// any name the transport offered, which is how a name becomes a permission.
pub fn may_act_as(conn: &Connection, proven: &str, claimed: &str) -> Result<()> {
    if proven.eq_ignore_ascii_case(claimed) {
        return Ok(());
    }
    let (proven_id, _, ceased) = pen_row(conn, proven)?;
    if ceased.is_some() {
        return Err(Error::Invalid(format!("{proven} has been retired")));
    }
    let claimed_id = exact_pen_id(conn, claimed)?.ok_or_else(|| {
        Error::Invalid(format!(
            "{claimed} is not a pen: a write may name itself or a pen it minted"
        ))
    })?;
    // (found reviewing #126) A CEASED PEN CANNOT BE CREDITED. Retire now ends its sessions, so this
    // is belt-and-braces — but a write credited to a pen that has ended would put new work in the
    // record under a name that can no longer answer for it, which is the one thing the ledger must
    // never do.
    let (_, _, claimed_ceased) = pen_row(conn, claimed)?;
    if claimed_ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{claimed} has been retired: a ceased pen cannot be credited for new work"
        )));
    }
    if !is_ancestor(conn, proven_id, claimed_id)? {
        return Err(Error::Invalid(format!(
            "{proven} may not write as {claimed}: a pen credits itself or its own subtree, never a \
             peer and never an ancestor"
        )));
    }
    // ★ PARENTAGE IS NECESSARY, NOT SUFFICIENT (James tie-break, 2026-10-23). Acting as a descendant
    // rides on a LIVE, EXPIRING lend — so a compromised finger's authority dies on its own, and a
    // revocation is one deleted row rather than a re-shaped tree. The check is uncached: a cached
    // session is a revocation that has not happened yet.
    if sudo::live(conn, claimed, proven)?.is_some() {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "{proven} may not write as {claimed} yet: that needs a LIVE lend — open one with `lend` (it \
         expires, and it can be revoked) rather than inheriting by tree shape"
    )))
}

/// Count what retiring `pen` would touch. Read-only; safe to call before deciding.
pub fn retire_preview(conn: &Connection, pen: &str) -> Result<RetirePreview> {
    let (id, parent_id, _) = pen_row(conn, pen)?;
    // ★ THE STORED SPELLING, NOT THE ARGUMENT (migration 0027). Lookups fold case, so `james` finds the
    // pen spelled `James` — and a preview that echoes what you typed is a preview of the wrong row.
    let stored: String =
        conn.query_row("SELECT name FROM pen WHERE id = ?1", params![id], |r| {
            r.get(0)
        })?;
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
        pen: stored,
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
    // ★ A ROOT PEN IS NOT RETIRABLE (James, 2026-10-23). `estate` is where terraform stands: no entity
    // behind it, no credential — a place identities may borrow, not a party that acts. "Retire the
    // root" is not an operation; the way to change the root is to change the configuration that
    // defines it. Saying that beats a "no pen named estate" error, which reads as a bug.
    let Some(parent_id) = parent_id else {
        return Err(Error::Invalid(format!(
            "{pen} is a root pen and is not retirable: the estate's own standing changes by configuration, not by an API call"
        )));
    };
    // Authorization: a parent — or any ancestor — retires its child.
    if !(caller_id == parent_id || is_ancestor(conn, caller_id, parent_id)?) {
        return Err(Error::Invalid(format!(
            "{caller} may not retire {pen}: only an ancestor may"
        )));
    }
    let parent = match rows {
        Rows::Inherit => Some(parent_id),
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
                delete_impl(conn, key, None, false)?;
            }
        }
        match history {
            History::Memorialize => {}
            History::Forget | History::Purge => {
                version::erase(conn, key)?;
            }
        }
    }

    // ★ DISPOSAL INCLUDES THE AUTHORITY IT WAS GIVEN (found reviewing #126). Retiring a pen left its
    // lends and borrows running until their TTL: a retired finger could still be credited, and a
    // retired hand still held what it borrowed. The tree's whole promise is that a disposal is
    // immediate, so every session naming this pen — as lender or borrower — ends here, and anything
    // passed down from one dies with it (`via`).
    conn.execute(
        "WITH RECURSIVE chain(id) AS (
             SELECT id FROM active_sudo WHERE powerful = ?1 OR weak = ?1
             UNION
             SELECT child.id FROM active_sudo child JOIN chain ON child.via = chain.id
         )
         DELETE FROM active_sudo WHERE id IN (SELECT id FROM chain)",
        params![id],
    )?;
    // ★ AND THE POLICY THAT AUTHORISED IT (found reviewing #127). A retired pen's `sudoers` row is
    // authority with no pen behind it; leaving it would let the next `sudo` borrow a ghost. The live
    // sessions themselves are ended a few lines up.
    conn.execute(
        "DELETE FROM sudoers WHERE powerful = ?1 OR weak = ?1",
        params![id],
    )?;
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
        "SELECT at, action,
                (SELECT name FROM pen WHERE id = from_pen),
                (SELECT name FROM pen WHERE id = to_pen),
                (SELECT name FROM pen WHERE id = pen_id)
           FROM pen_event WHERE key = ?1 ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![key], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One row nobody owns: the addressable half of the two counts in `memory_stats`.
///
/// ★ A NUMBER THAT CANNOT BECOME A LIST GETS QUOTED AND NEVER ACTED ON (Stet, review of #122). The
/// counts say how many; this says WHICH, so somebody can look at a row and claim it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unowned {
    /// The row's address.
    pub key: String,
    /// When it was written — the only ordering a claimer has to go on.
    pub created: String,
    /// Who typed it, as far as the store knows (`anonymous` when no currently existing pen is named).
    pub asserted_by: String,
    /// `disclaimed` — somebody RELEASED it, so the author is still named; or `unrecorded` — no author
    /// is named at all, which is either a write with no pen or a pen that is gone.
    pub why: String,
}

/// Every row with no owner, newest first, for somebody to claim.
///
/// ★ NOBODY OWNS IT IS A SUPPORTED STATE, NOT A BUG (James, 2026-10-23): when an ancestor retires a
/// descendant it chooses adopt / disclaim / delete, and a DISCLAIMED row is owned by nobody on
/// purpose — which is what "anonymous" means: no currently existing pen is answerable for it. So this
/// listing is not a damage report; it is the place an owner goes to pick something up, and `why`
/// keeps the two ways of arriving there apart.
pub fn unowned(conn: &Connection, reader: &audience::Reader, limit: i64) -> Result<Vec<Unowned>> {
    // ★ THE TIER APPLIES HERE TOO (found reviewing #122). The first version selected `owner IS NULL`
    // with no audience filter while its own doc said the tier applied "exactly as it does to a search
    // hit" — a control claimed and absent, which is the class this store keeps getting bitten by. No
    // live row is affected today (all 252 ownerless rows are public), so this is a gap closed before
    // it became a leak rather than a leak being plugged.
    let (pred, binds) = reader.sql();
    let mut stmt = conn.prepare(&format!(
        "SELECT key, created, asserted_by FROM memory
          WHERE owner IS NULL AND {pred}
          ORDER BY created DESC, key
          LIMIT ?{}",
        binds.len() + 1
    ))?;
    let mut all: Vec<String> = binds;
    all.push(limit.to_string());
    let rows = stmt.query_map(rusqlite::params_from_iter(all.iter()), |r| {
        let asserted_by: Option<String> = r.get(2)?;
        let asserted_by = asserted_by.unwrap_or_default();
        let why = if asserted_by.trim().is_empty()
            || asserted_by.eq_ignore_ascii_case(crate::identity::ANONYMOUS)
        {
            "unrecorded"
        } else {
            "disclaimed"
        };
        Ok(Unowned {
            key: r.get(0)?,
            created: r.get(1)?,
            asserted_by,
            why: why.to_string(),
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Delete a memory by key. `memory_link` follows via FK cascade; the vector is dropped explicitly, and
/// so is the key's queue entry. Returns whether a row was removed.
pub fn delete(conn: &Connection, key: &str, acted_by: Option<&str>) -> Result<bool> {
    guard_protected_delete(conn, key, acted_by)?;
    // ★ A DELETE IS A WRITE OF ABSENCE (soma/ellmephant#109). It takes the same reach as an edit —
    // the owner, the pen acting for the owner, or an ancestor — and it leaves the same kind of
    // record. Removal that nobody is named for is indistinguishable from a mistake.
    delete_impl(conn, key, acted_by, true)
}

/// The removal itself, once somebody has been found to answer for it.
///
/// `log` is false only for the caller that keeps its OWN record — `retire`, which already writes
/// "deleted on retirement" per row in the same act. One act, one ledger entry.
fn delete_impl(conn: &Connection, key: &str, acted_by: Option<&str>, log: bool) -> Result<bool> {
    let tx = conn.unchecked_transaction()?;
    // ★ THE GATE IS INSIDE THE TRANSACTION, for the same reason a compare-and-swap's compare is: a
    // check beside the write can be overtaken by an adopt or a disclaim between the two, and then the
    // removal proceeds under an authority that no longer holds. `retire` passes `log: false` because
    // it has already authorised the whole act by ancestry.
    if log {
        may_write(&tx, key, acted_by)?;
    }
    // The queue entry goes whatever the memory row does: an entry naming a row that no longer exists
    // is a document that can never be embedded, so leaving one behind is a leak the worker can only
    // rediscover. Both halves ride in one transaction, so neither undoes the other.
    encode::forget(&tx, &crate::store::links::Kind::Memory.keyslug(key))?;
    let row: Option<(i64, String, Option<i64>)> = tx
        .query_row(
            "SELECT id, audience, owner FROM memory WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let existed = row.is_some();
    if let Some((id, audience, owner)) = row {
        // A delete SUPERSEDES the state the memory is in, so it snapshots it first — exactly as an
        // edit does, and in the same transaction. Without this, a deleted key's newest version would
        // be some earlier state and the content it actually held would be the one thing versioning
        // did not keep; with it, `erase` is the store's single true destroyer.
        version::snapshot(&tx, id, key, None)?;
        tx.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
        // The ledger keeps WHAT HAPPENED even though the row is gone: an absence with no record is
        // the one deletion nobody can review.
        if log {
            // The actor is the pen that DID it — `exact_pen_id`, not `owner_pen_id`, which maps a
            // finger to its parent for ownership. Using the owner here recorded the hand for the
            // finger's deletions, which is the one distinction the ledger exists to keep.
            let actor = match acted_by {
                Some(name) => exact_pen_id(&tx, name)?,
                None => None,
            };
            log_pen_event(&tx, key, "delete", actor, owner, None, None)?;
        }
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
