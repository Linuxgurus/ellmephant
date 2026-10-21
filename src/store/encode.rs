//! The encode queue: the documents that still owe the vector index.
//!
//! A write stores the row and **schedules** its document in the same transaction; this module is the
//! other half — it turns a pending document into a vector. Splitting the two is the whole point: an
//! embed is the slow part of a write, and while it ran under the session lock, one slow write stalled
//! every other request. Here it runs holding nothing.
//!
//! **The queue carries the text, not a way to find it.** An entry is a keyslug (`kind:key`), the
//! *rendered* document, a priority and a time. So `claim` reads the queue and nothing else, which is
//! what lets one queue serve a memory, a project, or anything else that becomes worth embedding —
//! the worker never needs to know what a document *is*.
//!
//! **Failure keeps the entry.** It is the only record that a vector is owed: `missing()` asks whether
//! a vector EXISTS, and one describing older text does. So nothing removes an entry except a
//! successful settle; a caller steps over what it has just seen fail.
//!
//! **There is no stamp.** The dequeue guard is content equality: a write landing mid-embed replaces
//! the row's content, so the old job's DELETE misses and the newer text stays queued. That is the
//! same protection a hash gave, without a per-row copy of a fact about a derived index.
//!
//! Every path here is derived state. The rows in `memory` are the truth, so a queue entry that is
//! lost, duplicated or stale costs a re-embed and never a memory.

use rusqlite::{Connection, OptionalExtension, params};

use crate::embed::{Embedder, projection};
use crate::error::{Error, Result};
use crate::index;
use crate::store::links::Kind;

/// A write a human is waiting on: a fresh `memory_upsert`, an edit. Sorts ahead of bulk work.
pub const PRIORITY_LIVE: i32 = 3;

/// A rebuild. Sorted behind live work by [`PRIORITY_LIVE`] < [`PRIORITY_BULK`], so a re-encode of a
/// whole store never delays one edit — the reason `reembed` can be a producer of work instead of a
/// holder of the lock.
pub const PRIORITY_BULK: i32 = 5;

/// The schema version that introduced this table. A reader of an older store (the health probe, on a
/// half-migrated or rolled-back store) must not query it: a missing table would answer the question
/// "why is this store not ready?" with `no such table: encode_queue` instead of "schema behind binary".
pub const SCHEMA: i32 = 15;

/// One pending document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `kind:key` — where the document lives and what it is called.
    pub keyslug: String,
    /// The rendered text to embed, frozen when it was queued.
    pub content: String,
    /// Lower sorts first ([`PRIORITY_LIVE`], [`PRIORITY_BULK`]).
    pub priority: i32,
}

/// Schedule a document for embedding, by its rendered text.
///
/// Idempotent: one row per keyslug, so a burst of edits to one document is one embed of its latest
/// text. This is also the write path, so it is not the queue's "only producer" — a rebuild and the
/// refill rule are the other two. The priority is never DEMOTED — a live document swept up by a bulk re-encode stays live —
/// which is why this is an upsert with `min(priority, …)` and not `INSERT OR REPLACE` (REPLACE
/// resets every column it is not given, and would push a waiting write to the back).
pub fn enqueue(conn: &Connection, keyslug: &str, content: &str, priority: i32) -> Result<()> {
    // ★ THE ONE DEFINITION (soma/ellmephant#77): a document whose kind owes no vector must never
    // enter the queue. Queuing it would make the backlog report work that no worker can finish.
    if !is_embeddable(keyslug) {
        tracing::debug!(
            keyslug,
            "encode: this kind owes no vector by design — not queued"
        );
        return Ok(());
    }
    conn.execute(
        "INSERT INTO encode_queue (keyslug, content, priority) VALUES (?1, ?2, ?3)
         ON CONFLICT(keyslug) DO UPDATE SET
             content   = excluded.content,
             priority  = min(priority, excluded.priority),
             -- AGE MEANS \"the last time this document was ASKED for\", not \"first enqueued\": a
             -- re-request refreshes it. But a re-request carrying IDENTICAL text moves nothing, so it
             -- must not send the row to the back of its class either.
             queued_at = CASE WHEN content = excluded.content THEN queued_at ELSE datetime('now') END",
        params![keyslug, content, priority],
    )?;
    Ok(())
}

/// Schedule a **memory** by rendering its document first. The render is the kind's own decision
/// ([`projection::embed_document`]); the queue only ever sees the finished text.
pub fn enqueue_memory(
    conn: &Connection,
    key: &str,
    title: &str,
    body: &str,
    priority: i32,
) -> Result<()> {
    let content = projection::embed_document(key, title, body);
    enqueue(conn, &Kind::Memory.keyslug(key), &content, priority)
}

/// The next entry to do: live work before bulk work, oldest first inside a class.
///
/// No `ORDER BY rowid`: the timestamp is the order, and it is the only order the queue claims. Ties
/// inside a second are arbitrary, which is fine — the two came at about the same time.
/// `skip` names documents the caller has just seen fail. They are *stepped over*, not removed:
/// **an entry is the record that its document's vector is owed**, and deleting it on failure would
/// leave a row whose vector describes text that is no longer there, with nothing left to say so.
/// Keeping it means a stale vector stays visible — as a backlog that does not drain — until it is
/// settled. The skip is what stops that becoming a hot loop; it lives in the caller's memory, so a
/// restart retries immediately.
pub fn next(conn: &Connection, skip: &std::collections::HashSet<String>) -> Result<Option<Entry>> {
    let mut stmt = conn.prepare(
        "SELECT keyslug, content, priority FROM encode_queue
          ORDER BY priority, queued_at",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let entry = Entry {
            keyslug: row.get(0)?,
            content: row.get(1)?,
            priority: row.get(2)?,
        };
        if !skip.contains(&entry.keyslug) {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Drop a document's entry. Called when the row it names is gone — a deleted memory, or one merged
/// away — because a document that no longer exists must not be embedded.
pub fn forget(conn: &Connection, keyslug: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM encode_queue WHERE keyslug = ?1",
        params![keyslug],
    )?;
    Ok(())
}

/// A claimed document: everything needed to embed it, and to write the result back.
#[derive(Debug, Clone)]
pub struct Claim {
    keyslug: String,
    id: i64,
    doc: String,
    claimed_content: String,
}

impl Claim {
    /// The document the embedder should turn into a vector.
    pub fn doc(&self) -> &str {
        &self.doc
    }

    /// The keyslug this job belongs to — what a caller logs, and what a failure is recorded against.
    pub fn keyslug(&self) -> &str {
        &self.keyslug
    }
}

/// Take the head of `keyslug`'s entry without embedding anything: the **read half** of a job.
///
/// Split from the write half on purpose, and it is the whole design. An embed must run with the store
/// lock *released*, but SQLite has one writer and this crate keeps it behind one `Arc<Mutex<Session>>`
/// — so a caller that holds the lock claims here, drops it, embeds, and comes back to
/// [`settle_encoded`]. Burying all three in one function is precisely what would put the model call
/// back under the lock, which is the stall this module exists to remove.
///
/// The text comes from the QUEUE. Only the row's id is looked up, because a vector is keyed by it;
/// nothing is re-rendered, so the queue is the document's one description at that moment.
///
/// `None` when nothing is queued — or when the keyslug names no row (deleted, merged away, or a kind
/// that cannot be encoded yet), in which case the entry is dropped rather than left to haunt the queue.
pub fn claim(conn: &Connection, keyslug: &str) -> Result<Option<Claim>> {
    let content: Option<String> = conn
        .query_row(
            "SELECT content FROM encode_queue WHERE keyslug = ?1",
            params![keyslug],
            |r| r.get(0),
        )
        .optional()?;
    let Some(claimed_content) = content else {
        return Ok(None);
    };
    let Some((kind, key)) = Kind::from_keyslug(keyslug) else {
        // A keyslug nobody can address is not work; drop it rather than spin on it.
        forget(conn, keyslug)?;
        return Ok(None);
    };
    let id: Option<i64> = match kind {
        Kind::Memory => conn
            .query_row("SELECT id FROM memory WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?,
        // A project has no vector yet (0005: a structure is not matched by meaning). Queued work for
        // it is simply not doable, so the entry goes.
        Kind::Project => None,
    };
    let Some(id) = id else {
        forget(conn, keyslug)?;
        return Ok(None);
    };
    Ok(Some(Claim {
        keyslug: keyslug.to_string(),
        id,
        doc: claimed_content.clone(),
        claimed_content,
    }))
}

/// Write a claim's vector and clear its entry: the **write half** of a job.
///
/// The entry is settled *conditionally* — see [`settle`] — which is the guard that keeps a write
/// landing mid-embed from being swallowed.
pub fn settle_encoded(conn: &Connection, claim: &Claim, vector: &[f32]) -> Result<()> {
    // The vec table is created outside the transaction: DDL does not ride inside one.
    index::vec::ensure(conn, vector.len())?;
    let tx = conn.unchecked_transaction()?;
    let (kind, key) = Kind::from_keyslug(&claim.keyslug).ok_or_else(|| {
        Error::Config(format!(
            "claim has an unaddressable keyslug: {}",
            claim.keyslug
        ))
    })?;
    index::vec::upsert(&tx, claim.id, kind.as_str(), key, vector)?;
    // ★ NOTHING IS STAMPED ON THE ROW (2026-10-21). The vector lands and the queue entry goes; the
    // document itself records no encoding state, because a stamp describing a derived index is the
    // bug this whole refactor removes. "Is this current?" = "does any queue entry name it?".
    settle(&tx, &claim.keyslug, &claim.claimed_content)?;
    tx.commit()?;
    Ok(())
}

/// Embed one queued document, end to end. Returns whether there was an entry to do.
///
/// The composable form — [`claim`], the model call, [`settle_encoded`] — for a caller that holds *no*
/// store lock: tests, and the synchronous `?wait=true` door. A caller that holds the lock (the
/// worker) must not use this, or it holds the lock across the embed; it calls the two halves with the
/// embed in between.
pub fn encode_one(conn: &Connection, embedder: &dyn Embedder, keyslug: &str) -> Result<bool> {
    let Some(claimed) = claim(conn, keyslug)? else {
        return Ok(false);
    };
    let vector = embedder.embed_one(claimed.doc())?;
    settle_encoded(conn, &claimed, &vector)?;
    Ok(true)
}

/// Clear an entry, but only while it is still the one that was claimed — the guard.
///
/// A write that lands while a job is embedding replaces the row's `content`; this DELETE then misses,
/// and the newer text is embedded on the next pass instead of being silently dropped. Same protection
/// the old hash guard gave, and it costs no column.
fn settle(conn: &Connection, keyslug: &str, claimed_content: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM encode_queue WHERE keyslug = ?1 AND content = ?2",
        params![keyslug, claimed_content],
    )?;
    Ok(())
}

/// Encode up to `limit` entries, live work first; returns how many were attempted.
///
/// The synchronous door — `?wait=true` on a write, and every test that needs a vector *now*. The
/// worker does **not** use this: it composes [`claim`] and [`settle_encoded`] itself so it can drop
/// the store lock across the embed.
pub fn drain(conn: &Connection, embedder: &dyn Embedder, limit: usize) -> Result<usize> {
    let mut done = 0;
    let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();
    while done < limit {
        // A document that failed in THIS pass is stepped over, not removed: its entry is the record
        // that it still owes a vector.
        let Some(entry) = next(conn, &failed)? else {
            break;
        };
        if let Err(err) = encode_one(conn, embedder, &entry.keyslug) {
            failed.insert(entry.keyslug.clone());
            tracing::warn!(keyslug = %entry.keyslug, error = %err, "encode: embed failed, entry kept");
        }
        done += 1;
    }
    Ok(done)
}

/// Documents that have no vector: `(keyslug, rendered content)` for each memory whose id is absent
/// from the index.
///
/// This is the **refill rule's** input, and it is why nothing has to remember where a rebuild got to.
/// A crash, a dropped queue row, a hand-deleted vector and a completed re-encode all look the same
/// here — a document with no vector — so one mechanism repairs all of them, and it stops by itself
/// when the answer is empty.
pub fn missing(conn: &Connection) -> Result<Vec<(String, String)>> {
    // No index yet means every document is missing it — `ids` answers empty for a table that has
    // never been created, which is the same statement. There is no "fresh store" exception: a row with
    // no vector is a row with no vector, whatever the reason.
    let indexed: std::collections::HashSet<i64> = index::vec::ids(conn)?.into_iter().collect();
    let mut stmt = conn.prepare("SELECT id, key, title, body FROM memory ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(id, ..)| !indexed.contains(id))
        .map(|(_, key, title, body)| {
            (
                Kind::Memory.keyslug(&key),
                projection::embed_document(&key, &title, &body),
            )
        })
        .collect())
}

/// The queue, as a metric: the backlog and how long the oldest has waited.
///
/// The two questions a queue has to answer that nothing else can — *how much*, and *is it stuck* —
/// and neither is a readiness gate: with a durable queue a non-empty backlog is NORMAL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    /// Documents still owing a vector.
    pub pending: i64,
    /// Entries that have given up. **Always `0` since 0013**: the queue keeps no failure history — a
    /// failed embed drops its entry and the refill rule is the retry — so there is no such row to
    /// count. Kept until the wire drops the field.
    pub failed: i64,
    /// Age of the oldest pending entry, in seconds — `None` when nothing is queued. A backlog that
    /// only grows here is a worker that is not running, which nothing else would say.
    pub oldest_s: Option<i64>,
}

/// Whether a keyslug names a document that owes a vector — the queue's side of the ONE definition
/// ([`Kind::is_embeddable`]). An unaddressable keyslug is not a document: the claim drops it.
pub fn is_embeddable(keyslug: &str) -> bool {
    match Kind::from_keyslug(keyslug) {
        Some((kind, _)) => kind.is_embeddable(),
        None => true, // not addressable, so not ours to judge; `claim` forgets it
    }
}

/// How many queued documents genuinely OWE a vector.
///
/// This is the health predicate's question, and it is not `count(*)`: a queued document whose kind is
/// not embeddable owes nothing, so counting the row would report a rebuild that no amount of work can
/// finish. The other half of the old answer — `vectors < documents` — is worse, because it treats a
/// lexical-only or non-embedded document as permanently owed. See [`Kind::is_embeddable`].
pub fn owed(conn: &Connection) -> Result<i64> {
    let mut stmt = conn.prepare("SELECT keyslug FROM encode_queue")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows
        .filter_map(|r| r.ok())
        .filter(|slug| is_embeddable(slug))
        .count() as i64)
}

/// The one verdict on the vector index, computed in one place for `/health` and `doctor` — they had
/// two copies of this chain and could drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    /// The stored vectors were built from a different projection and must be re-embedded.
    pub stale: bool,
    /// Queued documents that actually owe a vector ([`owed`]).
    pub owed: i64,
    /// A projection fingerprint has been recorded at all.
    pub recorded: bool,
}

impl Projection {
    /// The verdict, in the words both surfaces print. `vectors` and `documents` are for the
    /// REBUILDING message only — never for the decision.
    pub fn verdict(&self, vectors: i64, documents: i64) -> String {
        if self.stale {
            "STALE — run `reembed`".to_string()
        } else if self.owed > 0 {
            format!(
                "REBUILDING — {vectors} of {documents} document(s) indexed, {} queued",
                self.owed
            )
        } else if self.recorded {
            "current".to_string()
        } else {
            "unrecorded".to_string()
        }
    }
}

/// Read the index's verdict. `stale` is the fingerprint comparison; `owed` is the queue's real debt.
pub fn projection(conn: &Connection) -> Result<Projection> {
    Ok(Projection {
        stale: index::vec::projection_stale(conn)?,
        owed: owed(conn)?,
        recorded: crate::db::meta::get(conn, crate::db::meta::PROJECTION)?.is_some(),
    })
}

/// Read the queue's shape in one aggregate, so a caller cannot get a count that disagrees with
/// another.
pub fn counts(conn: &Connection) -> Result<Counts> {
    Ok(conn.query_row(
        "SELECT count(*),
                CAST(strftime('%s', 'now') - strftime('%s', min(queued_at)) AS INTEGER)
           FROM encode_queue",
        [],
        |r| {
            Ok(Counts {
                pending: r.get(0)?,
                failed: 0,
                oldest_s: r.get(1)?,
            })
        },
    )?)
}

/// How many documents still owe a vector — the encoder's backlog.
pub fn pending(conn: &Connection) -> Result<i64> {
    Ok(counts(conn)?.pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::db;
    use crate::embed::HashEmbedder;
    use crate::error::Error;
    use crate::store::Memory;

    /// A throwaway store. The queue is a table, so most of its behaviour is only observable through
    /// SQL — and a real file is the only honest way to see the guard behave.
    fn store() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_root(dir.path());
        let conn = db::open(&paths).expect("open");
        (dir, conn)
    }

    fn mem(key: &str, body: &str) -> Memory {
        Memory {
            key: key.to_string(),
            title: key.to_string(),
            body: body.to_string(),
            ..Default::default()
        }
    }

    fn slug(key: &str) -> String {
        Kind::Memory.keyslug(key)
    }

    /// Nothing is skipped: the common case for a queue's tests.
    fn none() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    /// A stored row that OWES a vector. Under the queue that is simply what a write leaves behind:
    /// `add` schedules the document and embeds nothing. A real write, not a fixture that fakes it.
    fn queue_owed(conn: &Connection, embedder: &dyn Embedder, key: &str, body: &str) {
        let _ = embedder; // the write no longer needs a model: it queues rendered text
        crate::store::add(conn, &mem(key, body)).expect("add");
    }

    fn id_of(conn: &Connection, key: &str) -> i64 {
        conn.query_row("SELECT id FROM memory WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .expect("row id")
    }

    #[test]
    fn a_keyslug_says_which_table_and_which_name() {
        assert_eq!(Kind::Memory.keyslug("sessions"), "memory:sessions");
        assert_eq!(Kind::Project.keyslug("sessions"), "project:sessions");
        assert_eq!(
            Kind::from_keyslug("project:sessions"),
            Some((Kind::Project, "sessions"))
        );
        assert_eq!(
            Kind::from_keyslug("memory:sessions"),
            Some((Kind::Memory, "sessions"))
        );
        assert_eq!(Kind::from_keyslug("sessions"), None, "no type, no keyslug");
        assert_eq!(
            Kind::from_keyslug("prj:sessions"),
            None,
            "prj is a reference, not a table"
        );
        assert_eq!(
            Kind::from_keyslug("memory:"),
            None,
            "an empty key names nothing"
        );
    }

    #[test]
    fn enqueue_coalesces_into_one_entry_for_the_latest_text() {
        let (_d, conn) = store();
        enqueue(&conn, &slug("a"), "doc-1", PRIORITY_LIVE).unwrap();
        enqueue(&conn, &slug("a"), "doc-2", PRIORITY_LIVE).unwrap();

        assert_eq!(
            pending(&conn).unwrap(),
            1,
            "a re-enqueue is not a second job"
        );
        let entry = next(&conn, &none()).unwrap().expect("an entry");
        assert_eq!(entry.keyslug, slug("a"));
        assert_eq!(
            entry.content, "doc-2",
            "the LATEST text is what gets embedded"
        );
    }

    #[test]
    fn bulk_sorts_behind_live_work_and_never_demotes_it() {
        let (_d, conn) = store();
        enqueue(&conn, &slug("bulk"), "doc", PRIORITY_BULK).unwrap();
        enqueue(&conn, &slug("live"), "doc", PRIORITY_LIVE).unwrap();
        assert_eq!(
            next(&conn, &none()).unwrap().unwrap().keyslug,
            slug("live"),
            "a live write goes first"
        );

        // A bulk re-encode sweeping up a document a human is waiting on must not push it back.
        enqueue(&conn, &slug("live"), "doc-2", PRIORITY_BULK).unwrap();
        let live = next(&conn, &none()).unwrap().unwrap();
        assert_eq!(live.keyslug, slug("live"));
        assert_eq!(live.priority, PRIORITY_LIVE, "priority is never demoted");
        assert_eq!(live.content, "doc-2", "but the text is the newest one");
    }

    #[test]
    fn encode_one_writes_the_vector_and_settles_the_entry() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        queue_owed(&conn, &embedder, "a", "alpha");
        let id = id_of(&conn, "a");
        assert!(
            index::vec::get(&conn, id).unwrap().is_none(),
            "precondition"
        );

        assert!(encode_one(&conn, &embedder, &slug("a")).unwrap());

        assert_eq!(pending(&conn).unwrap(), 0, "the entry settled");
        assert!(
            index::vec::get(&conn, id).unwrap().is_some(),
            "the vector landed"
        );
    }

    #[test]
    fn encode_one_reports_no_work_for_an_unknown_or_settled_keyslug() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        queue_owed(&conn, &embedder, "a", "alpha");

        assert!(encode_one(&conn, &embedder, &slug("a")).unwrap());
        assert!(
            !encode_one(&conn, &embedder, &slug("a")).unwrap(),
            "the entry is gone, so there is nothing to do"
        );
        assert!(!encode_one(&conn, &embedder, &slug("never-queued")).unwrap());
        assert_eq!(pending(&conn).unwrap(), 0);
    }

    /// The guard. A write that lands while a job is embedding replaces the entry's `content`; the
    /// job's dequeue must then MISS, or the newer text is never indexed.
    #[test]
    fn an_entry_that_moved_on_is_not_settled_by_the_job_that_claimed_it() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        queue_owed(&conn, &embedder, "a", "alpha");
        let claimed = claim(&conn, &slug("a")).unwrap().expect("a claim");
        let claimed_content = claimed.doc().to_string();

        // A write lands after the claim: the row's text moved.
        enqueue(&conn, &slug("a"), "newer text", PRIORITY_LIVE).unwrap();

        settle(&conn, &slug("a"), &claimed_content).unwrap();
        assert_eq!(pending(&conn).unwrap(), 1, "the newer text is still owed");
        assert_eq!(next(&conn, &none()).unwrap().unwrap().content, "newer text");

        settle(&conn, &slug("a"), "newer text").unwrap();
        assert_eq!(pending(&conn).unwrap(), 0);
    }

    #[test]
    fn an_entry_whose_row_is_gone_is_dropped_rather_than_left_haunting_the_queue() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        enqueue(&conn, &slug("ghost"), "nothing", PRIORITY_LIVE).unwrap();

        assert!(!encode_one(&conn, &embedder, &slug("ghost")).unwrap());
        assert_eq!(pending(&conn).unwrap(), 0, "nothing to embed, nothing owed");
    }

    /// ★ soma/ellmephant#77: the ONE definition, and the reason it is not `count(*)`. A project is a
    /// structure and owes no vector (0005, and `claim` drops its work), so a queued project is a row
    /// in the backlog that no worker can ever finish. `owed` must not count it, and `enqueue` must
    /// not create it — otherwise `/health` reports a rebuild that will never clear.
    #[test]
    fn a_kind_that_owes_no_vector_is_neither_queued_nor_owed() {
        let (_d, conn) = store();
        // A legacy/foreign row of the non-embeddable kind.
        conn.execute(
            "INSERT INTO encode_queue (keyslug, content, priority) VALUES ('project:sessions', 'x', 3)",
            [],
        )
        .unwrap();
        assert_eq!(pending(&conn).unwrap(), 1, "it is in the queue, as a row");
        assert_eq!(owed(&conn).unwrap(), 0, "but it owes nothing");

        // The writer refuses to add another: the definition is shared, not re-derived.
        enqueue(&conn, "project:sessions", "y", PRIORITY_LIVE).unwrap();
        assert_eq!(pending(&conn).unwrap(), 1, "still one row, and unchanged");

        enqueue(&conn, &slug("a"), "doc", PRIORITY_LIVE).unwrap();
        assert_eq!(owed(&conn).unwrap(), 1, "a memory IS owed");

        // A drained queue over documents that mostly cannot be embedded is `current`, not
        // REBUILDING — the acceptance in #77.
        let p = Projection {
            stale: false,
            owed: 0,
            recorded: true,
        };
        assert_eq!(p.verdict(1, 40), "current");
        let p = Projection {
            stale: false,
            owed: 0,
            recorded: false,
        };
        assert_eq!(p.verdict(0, 0), "unrecorded");
    }

    #[test]
    fn an_unaddressable_keyslug_is_dropped() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        enqueue(&conn, "rubbish", "nothing", PRIORITY_LIVE).unwrap();

        assert!(!encode_one(&conn, &embedder, "rubbish").unwrap());
        assert_eq!(pending(&conn).unwrap(), 0);
    }

    #[test]
    fn drain_empties_the_queue_and_reports_how_much_it_did() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        queue_owed(&conn, &embedder, "a", "alpha");
        queue_owed(&conn, &embedder, "b", "beta");
        queue_owed(&conn, &embedder, "c", "gamma");

        assert_eq!(pending(&conn).unwrap(), 3);
        assert_eq!(drain(&conn, &embedder, 10).unwrap(), 3);
        assert_eq!(pending(&conn).unwrap(), 0);
        for key in ["a", "b", "c"] {
            assert!(
                index::vec::get(&conn, id_of(&conn, key)).unwrap().is_some(),
                "{key} is searchable"
            );
        }
        // A drain on an empty queue is a no-op, not an error.
        assert_eq!(drain(&conn, &embedder, 10).unwrap(), 0);
    }

    /// The two halves are the worker's contract: a caller holding the store lock claims, drops the
    /// lock to embed, and settles afterwards. The text crosses the gap in the claim itself.
    #[test]
    fn claim_and_settle_carry_a_job_across_the_embed() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        queue_owed(&conn, &embedder, "a", "alpha");

        let claimed = claim(&conn, &slug("a")).unwrap().expect("a claim");
        let vector = embedder.embed_one(claimed.doc()).unwrap();
        settle_encoded(&conn, &claimed, &vector).unwrap();
        assert_eq!(pending(&conn).unwrap(), 0);
        assert!(index::vec::get(&conn, id_of(&conn, "a")).unwrap().is_some());
    }

    /// **An entry is the record that a vector is owed.** So a failure keeps it: deleting it would
    /// leave the row with an OLD vector and nothing left to say so, and `missing()` could not catch
    /// it either — a vector exists. That is the silent-staleness hole a per-row hash used to cover.
    #[test]
    fn a_failed_embed_keeps_its_entry_so_the_staleness_stays_visible() {
        struct Broken;
        impl Embedder for Broken {
            fn dim(&self) -> usize {
                8
            }
            fn model_id(&self) -> &str {
                "broken"
            }
            fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
                Err(Error::Config("no vector for you".into()))
            }
        }

        let (_d, conn) = store();
        let real = HashEmbedder::new(8);
        queue_owed(&conn, &real, "a", "alpha");
        let id = id_of(&conn, "a");
        drain(&conn, &real, 10).unwrap();
        let current = index::vec::get(&conn, id).unwrap().expect("indexed");
        assert_eq!(missing(&conn).unwrap().len(), 0, "nothing is missing");

        // The document moves on, and the embed for the new text fails.
        crate::store::add(&conn, &mem("a", "alpha, revised")).unwrap();
        assert_eq!(drain(&conn, &Broken, 10).unwrap(), 1);

        // The vector is now STALE, and nothing about the index would say so: the vector exists, so
        // `missing` is empty. The QUEUE is the only record of the debt — which is the whole point.
        assert_eq!(
            index::vec::get(&conn, id).unwrap().unwrap(),
            current,
            "the vector still describes the previous text"
        );
        assert_eq!(
            missing(&conn).unwrap().len(),
            0,
            "and the refill rule cannot see it, because a vector exists"
        );
        assert_eq!(pending(&conn).unwrap(), 1, "the debt stays on the books");
        assert_eq!(
            next(&conn, &none()).unwrap().unwrap().content,
            projection::embed_document("a", "a", "alpha, revised"),
            "the entry names the text that is owed"
        );

        // A working embedder settles it, and the staleness goes.
        assert_eq!(drain(&conn, &real, 10).unwrap(), 1);
        assert_eq!(pending(&conn).unwrap(), 0);
        assert_ne!(index::vec::get(&conn, id).unwrap().unwrap(), current);
    }

    /// A document that keeps failing must not wedge the queue: the worker steps over what it has just
    /// seen fail, so the next document is still served. The skip lives in the caller, so a restart
    /// tries again at once.
    #[test]
    fn the_queue_steps_over_a_document_that_has_just_failed() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        queue_owed(&conn, &embedder, "a", "alpha");
        queue_owed(&conn, &embedder, "b", "beta");

        let head = next(&conn, &none()).unwrap().unwrap();
        let mut skip = std::collections::HashSet::new();
        skip.insert(head.keyslug.clone());
        let after = next(&conn, &skip).unwrap().expect("the next document");
        assert_ne!(after.keyslug, head.keyslug);
        assert_eq!(pending(&conn).unwrap(), 2, "nothing was removed");
        assert_eq!(drain(&conn, &embedder, 10).unwrap(), 2);
    }

    /// The two numbers a queue has to answer with — how much, and is it stuck — from one aggregate.
    #[test]
    fn counts_reports_the_backlog_and_the_oldest_wait() {
        let (_d, conn) = store();
        let _embedder = HashEmbedder::new(8);

        assert_eq!(
            counts(&conn).unwrap(),
            Counts::default(),
            "an empty queue is zeroes and no age at all"
        );

        crate::store::add(&conn, &mem("a", "alpha")).unwrap();
        let c = counts(&conn).unwrap();
        assert_eq!(c.pending, 1);
        assert_eq!(
            c.oldest_s,
            Some(0),
            "queued this second, so zero seconds old"
        );
        assert_eq!(pending(&conn).unwrap(), c.pending, "the heads agree");
    }

    /// `missing` is the refill rule's input: it names documents with no vector, and it is the reason
    /// nothing has to remember where a rebuild got to.
    #[test]
    fn missing_names_the_documents_that_have_no_vector() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);

        // Written but not yet embedded: no vector, so it is missing — with no index at all, which is
        // the same answer `ids` gives for a table that has never been created.
        crate::store::add(&conn, &mem("a", "alpha")).unwrap();
        assert_eq!(
            missing(&conn).unwrap().len(),
            1,
            "written and not embedded is missing, index or no index"
        );

        drain(&conn, &embedder, 10).unwrap();
        assert_eq!(missing(&conn).unwrap().len(), 0, "all indexed");

        // A vector removed by hand is exactly the shape of a torn index.
        conn.execute(
            "DELETE FROM document_vec WHERE vector_id = ?1",
            [id_of(&conn, "a")],
        )
        .unwrap();
        let m = missing(&conn).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, slug("a"), "the keyslug, not the bare key");
        assert!(
            m[0].1.contains("alpha"),
            "and the rendered text, so the queue needs no truth read"
        );
    }

    /// The P2 contract in one place: a **write queues the document and writes no vector**.
    #[test]
    fn a_write_queues_the_document_and_writes_no_vector() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);

        crate::store::add(&conn, &mem("a", "alpha")).unwrap();

        assert_eq!(pending(&conn).unwrap(), 1, "the write queued a document");
        assert!(
            index::vec::get(&conn, id_of(&conn, "a")).unwrap().is_none(),
            "nothing has been embedded yet"
        );
        assert_eq!(
            next(&conn, &none()).unwrap().unwrap().keyslug,
            slug("a"),
            "and the entry is for exactly this document"
        );

        // Honestly embedded now, so a tag-only edit can be judged at all: a tag is FILING, not
        // meaning, so it moves no document and therefore queues nothing (SPEC §6/§9).
        drain(&conn, &embedder, 10).unwrap();
        let tagged = crate::store::merge(
            &conn,
            &crate::store::Patch {
                key: Some("a".into()),
                tags: Some(vec!["k8s".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!tagged.queued, "a tag is filing, not meaning");
        assert_eq!(pending(&conn).unwrap(), 0);

        // The body IS the document, so that one queues.
        let edited = crate::store::merge(
            &conn,
            &crate::store::Patch {
                key: Some("a".into()),
                body: Some("changed".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(edited.queued, "the body is the document");
        assert_eq!(pending(&conn).unwrap(), 1);
        // ★ AND THE ROW SAYS NOTHING ABOUT IT (2026-10-21): the queue is the only record that a vector
        // is owed. There is no `encode_hash` column to read, by design — a stamp describing a derived
        // index does not belong on the document.
    }
}
