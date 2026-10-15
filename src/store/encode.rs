//! The encode queue: the documents that still owe the vector index.
//!
//! A write stores the row and **schedules** its document in the same transaction; this module is the
//! other half — it turns a pending document into a vector. Splitting the two is the whole point: an
//! embed is the slow part of a write, and while it ran under the session lock, one slow write stalled
//! every other request. Here it runs holding nothing.
//!
//! Two functions carry it. [`encode_one`] does one document and is idempotent by construction — the
//! work is a model call plus a small transaction, and the dequeue is conditional on the hash it was
//! claimed for, so a write landing mid-embed is *kept*, not swallowed. [`drain`] is `encode_one` in a
//! loop, which is why the worker needs no second implementation: it is this loop with a sleep in it.
//!
//! Every path here is derived state. The rows in `memory` are the truth, so a queue entry that is
//! lost, duplicated or stale costs a re-embed and never a memory.

use rusqlite::{Connection, OptionalExtension, params};

use crate::embed::{Embedder, VecSpec, projection};
use crate::error::{Error, Result};
use crate::index;
use crate::store::row_hash;

/// The encodable kind for a memory. A value in a column, not a constant in the schema, so a project
/// — or anything else that turns out to be worth embedding — can share this queue rather than
/// getting a table of its own.
pub const KIND_MEMORY: &str = "memory";

/// A write a human is waiting on: a fresh `memory_upsert`, an edit. Sorts first.
pub const INTERACTIVE: i32 = 0;

/// A rebuild. Sorts behind anything interactive, so a backfill of a whole store never delays one
/// edit — the reason a `reembed` can be a producer of work instead of a holder of the lock.
pub const BACKFILL: i32 = 1;

/// How many times one entry is retried before it is left alone. A safety valve: an entry that fails
/// this often is surfaced by [`failed`] instead of being retried in a hot loop forever.
pub const MAX_ATTEMPTS: i64 = 5;

/// One pending document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The memory this document belongs to.
    pub key: String,
    /// What is being embedded ([`KIND_MEMORY`] today).
    pub kind: String,
    /// Lower sorts first ([`INTERACTIVE`], [`BACKFILL`]).
    pub priority: i32,
    /// The document hash this entry was created for — the dequeue guard.
    pub doc_hash: String,
    /// Failed attempts so far.
    pub attempts: i64,
}

/// Schedule `key`'s document for embedding. Idempotent: an entry for the same key is *updated*, never
/// duplicated, so a burst of edits to one memory is one embed of the latest text.
///
/// The priority is never demoted — an interactive key re-enqueued by a backfill stays interactive,
/// which is what keeps a rebuild from pushing a live edit to the back of its own queue.
pub fn enqueue(conn: &Connection, key: &str, doc_hash: &str, priority: i32) -> Result<()> {
    conn.execute(
        "INSERT INTO encode_queue (key, kind, priority, doc_hash) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(key) DO UPDATE SET
             doc_hash   = excluded.doc_hash,
             priority   = min(priority, excluded.priority),
             queued_at  = datetime('now'),
             attempts   = 0,
             last_error = NULL",
        params![key, KIND_MEMORY, priority, doc_hash],
    )?;
    Ok(())
}

/// How many documents still owe a vector — the encoder's backlog.
pub fn pending(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM encode_queue", [], |r| r.get(0))?)
}

/// How many entries have exhausted [`MAX_ATTEMPTS`] and are being skipped. Non-zero means something
/// is failing in a way a retry will not fix, and it is deliberately visible rather than silent.
pub fn failed(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM encode_queue WHERE attempts >= ?1",
        params![MAX_ATTEMPTS],
        |r| r.get(0),
    )?)
}

/// The next entry to do: lowest priority first, then oldest, then key for a stable order.
///
/// An entry past [`MAX_ATTEMPTS`] is skipped, not returned — the loop must not wedge on one document
/// that cannot be embedded. It stays in the table for [`failed`] to report.
pub fn next(conn: &Connection) -> Result<Option<Entry>> {
    Ok(conn
        .query_row(
            "SELECT key, kind, priority, doc_hash, attempts FROM encode_queue
          WHERE attempts < ?1
          ORDER BY priority, queued_at, key
          LIMIT 1",
            params![MAX_ATTEMPTS],
            |r| {
                Ok(Entry {
                    key: r.get(0)?,
                    kind: r.get(1)?,
                    priority: r.get(2)?,
                    doc_hash: r.get(3)?,
                    attempts: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Drop a key's entry. Called when the row it names is gone — a deleted memory, or one that was
/// merged away — because a document that no longer exists must not be embedded.
pub fn forget(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM encode_queue WHERE key = ?1", params![key])?;
    Ok(())
}

/// A claimed document: everything needed to embed it, and to write the result back.
#[derive(Debug, Clone)]
pub struct Claim {
    key: String,
    id: i64,
    doc: String,
    doc_hash: String,
    claimed_hash: String,
    indexed: bool,
}

impl Claim {
    /// The document the embedder should turn into a vector.
    pub fn doc(&self) -> &str {
        &self.doc
    }

    /// Whether the model call is needed at all: false when the row already carries this document's
    /// vector (a duplicate job, or a re-enqueue whose text never moved), in which case the entry is
    /// still settled but nothing is embedded.
    pub fn needs_embed(&self) -> bool {
        !self.indexed
    }
}

/// Take the head of `key`'s entry without embedding anything: the **read half** of a job.
///
/// Split from the write half on purpose, and it is the whole design. An embed must run with the store
/// lock *released*, but SQLite has one writer and this crate keeps it behind one `Arc<Mutex<Session>>`
/// — so a caller that holds the lock claims here, drops it, embeds, and comes back to
/// [`settle_encoded`]. Burying all three in one function is precisely what would put the model call
/// back under the lock, which is the stall this module exists to remove.
///
/// `None` when there is no entry — or when the row it names is gone, in which case the entry is
/// dropped rather than left to haunt the queue.
pub fn claim(conn: &Connection, spec: &VecSpec, key: &str) -> Result<Option<Claim>> {
    // The row as it stands NOW, and the hash the entry was created for. Both matter: the first is
    // what to embed, the second is what to clear.
    let row: Option<(i64, String, String, String, String)> = conn
        .query_row(
            "SELECT m.id, m.title, m.body, m.doc_hash, q.doc_hash
               FROM encode_queue q JOIN memory m ON m.key = q.key
              WHERE q.key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((id, title, body, indexed_hash, claimed_hash)) = row else {
        forget(conn, key)?;
        return Ok(None);
    };
    let doc = projection::embed_document(key, &title, &body);
    let doc_hash = row_hash(spec, &doc);
    Ok(Some(Claim {
        key: key.to_string(),
        id,
        indexed: indexed_hash == doc_hash,
        doc,
        doc_hash,
        claimed_hash,
    }))
}

/// Write a claim's vector and clear its entry: the **write half** of a job. `vector` is `None`
/// exactly when the claim needed no embed.
///
/// Either way the entry is settled *conditionally* — see [`settle`] — which is the guard that keeps a
/// write landing mid-embed from being swallowed.
pub fn settle_encoded(conn: &Connection, claim: &Claim, vector: Option<&[f32]>) -> Result<()> {
    if claim.indexed {
        return settle(conn, &claim.key, &claim.claimed_hash);
    }
    let vector = vector.ok_or_else(|| {
        Error::Config(format!(
            "claim {:?} still needs a vector; settle_encoded was called without one",
            claim.key
        ))
    })?;
    // The vec table is created outside the transaction: DDL does not ride inside one.
    index::vec::ensure(conn, vector.len())?;
    let tx = conn.unchecked_transaction()?;
    index::vec::upsert(&tx, claim.id, vector)?;
    // `doc_hash` advances only HERE, beside the vector it describes: the column means "the text whose
    // vector is stored", so the row write must never move it — that is what makes a pending row
    // detectable at all.
    tx.execute(
        "UPDATE memory SET doc_hash = ?2 WHERE id = ?1",
        params![claim.id, claim.doc_hash],
    )?;
    settle(&tx, &claim.key, &claim.claimed_hash)?;
    tx.commit()?;
    Ok(())
}

/// Embed one queued document, end to end. Returns whether there was an entry to do.
///
/// The composable form — [`claim`], the model call, [`settle_encoded`] — for a caller that holds *no*
/// store lock: tests, and the synchronous `?wait=true` door. A caller that holds the lock (the
/// worker) must not use this, or it holds the lock across the embed; it calls the two halves with the
/// embed in between.
pub fn encode_one(
    conn: &Connection,
    embedder: &dyn Embedder,
    spec: &VecSpec,
    key: &str,
) -> Result<bool> {
    let Some(claim) = claim(conn, spec, key)? else {
        return Ok(false);
    };
    let vector = match claim.needs_embed() {
        true => Some(embedder.embed_one(claim.doc())?),
        false => None,
    };
    settle_encoded(conn, &claim, vector.as_deref())?;
    Ok(true)
}

/// Clear an entry, but only while it is still the one that was claimed — the guard.
fn settle(conn: &Connection, key: &str, claimed_hash: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM encode_queue WHERE key = ?1 AND doc_hash = ?2",
        params![key, claimed_hash],
    )?;
    Ok(())
}

/// Note a failed attempt. The entry stays, so a transient failure is simply retried — until
/// [`MAX_ATTEMPTS`] gives up on it and [`failed`] reports it instead of looping forever.
fn record_failure(conn: &Connection, key: &str, err: &Error) -> Result<()> {
    conn.execute(
        "UPDATE encode_queue SET attempts = attempts + 1, last_error = ?2 WHERE key = ?1",
        params![key, err.to_string()],
    )?;
    Ok(())
}

/// Encode up to `limit` entries, highest priority first; returns how many were processed.
///
/// The synchronous door — `?wait=true` on a write, and every test that needs a vector *now* — and it
/// is the same code the worker runs, minus the sleep. One implementation, two drivers.
///
/// The worker does **not** use this: it composes [`claim`] and [`settle_encoded`] itself, so it can
/// drop the store lock across the embed. That also gives it a different retry shape — one attempt per
/// pass, with the sleep as the backoff — where a wide `drain` burns the retries back to back. That is
/// fine for the synchronous door: each failure is a local error, and [`MAX_ATTEMPTS`] bounds the total.
pub fn drain(
    conn: &Connection,
    embedder: &dyn Embedder,
    spec: &VecSpec,
    limit: usize,
) -> Result<usize> {
    let mut done = 0;
    while done < limit {
        let Some(entry) = next(conn)? else { break };
        if let Err(err) = encode_one(conn, embedder, spec, &entry.key) {
            // One bad document must not stop the queue; it is counted and left for the retry.
            record_failure(conn, &entry.key, &err)?;
        }
        done += 1;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::db;
    use crate::embed::HashEmbedder;
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

    /// The document hash of `body` under `key`, as the write path would compute it.
    fn doc(key: &str, body: &str, spec: &VecSpec) -> String {
        row_hash(spec, &projection::embed_document(key, key, body))
    }

    /// A stored row that OWES a vector: written normally, then stripped back to "never embedded",
    /// which is precisely the state the queue exists to serve.
    fn owed(conn: &Connection, embedder: &dyn Embedder, key: &str, body: &str) -> VecSpec {
        let spec = VecSpec::of(embedder);
        let (id, _) = crate::store::add(conn, embedder, &mem(key, body)).expect("add");
        index::vec::delete(conn, id).expect("unindex");
        conn.execute("UPDATE memory SET doc_hash = '' WHERE id = ?1", params![id])
            .expect("clear doc_hash");
        spec
    }

    fn id_of(conn: &Connection, key: &str) -> i64 {
        conn.query_row("SELECT id FROM memory WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .expect("row id")
    }

    fn stored_hash(conn: &Connection, key: &str) -> String {
        conn.query_row(
            "SELECT doc_hash FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .expect("doc_hash")
    }

    #[test]
    fn enqueue_coalesces_into_one_entry_for_the_latest_text() {
        let (_d, conn) = store();
        enqueue(&conn, "a", "hash-1", INTERACTIVE).unwrap();
        enqueue(&conn, "a", "hash-2", INTERACTIVE).unwrap();

        assert_eq!(
            pending(&conn).unwrap(),
            1,
            "a re-enqueue is not a second job"
        );
        let entry = next(&conn).unwrap().expect("an entry");
        assert_eq!(entry.key, "a");
        assert_eq!(
            entry.doc_hash, "hash-2",
            "the LATEST text is what gets embedded"
        );
        assert_eq!(entry.kind, KIND_MEMORY);
    }

    #[test]
    fn a_backfill_sorts_behind_an_interactive_write_and_never_demotes_one() {
        let (_d, conn) = store();
        enqueue(&conn, "bulk", "h", BACKFILL).unwrap();
        enqueue(&conn, "live", "h", INTERACTIVE).unwrap();
        assert_eq!(
            next(&conn).unwrap().unwrap().key,
            "live",
            "interactive first"
        );

        // A backfill re-enqueue of a key a human is waiting on must not push it to the back.
        enqueue(&conn, "live", "h2", BACKFILL).unwrap();
        let live = next(&conn).unwrap().unwrap();
        assert_eq!(live.key, "live");
        assert_eq!(live.priority, INTERACTIVE, "priority is never demoted");
        assert_eq!(live.doc_hash, "h2", "but the text is the newest one");
    }

    #[test]
    fn encode_one_writes_the_vector_and_settles_the_entry() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        let spec = owed(&conn, &embedder, "a", "alpha");
        let id = id_of(&conn, "a");
        enqueue(&conn, "a", &doc("a", "alpha", &spec), INTERACTIVE).unwrap();
        assert!(
            index::vec::get(&conn, id).unwrap().is_none(),
            "precondition"
        );

        assert!(encode_one(&conn, &embedder, &spec, "a").unwrap());

        assert_eq!(pending(&conn).unwrap(), 0, "the entry settled");
        assert!(
            index::vec::get(&conn, id).unwrap().is_some(),
            "the vector landed"
        );
        assert_eq!(
            stored_hash(&conn, "a"),
            doc("a", "alpha", &spec),
            "doc_hash advances with the vector, and only then"
        );
    }

    #[test]
    fn encode_one_is_idempotent_and_reports_no_work_for_an_unknown_key() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        let spec = owed(&conn, &embedder, "a", "alpha");
        enqueue(&conn, "a", &doc("a", "alpha", &spec), INTERACTIVE).unwrap();

        assert!(encode_one(&conn, &embedder, &spec, "a").unwrap());
        assert!(
            !encode_one(&conn, &embedder, &spec, "a").unwrap(),
            "the entry is gone, so there is nothing to do"
        );
        assert!(!encode_one(&conn, &embedder, &spec, "never-queued").unwrap());
        assert_eq!(pending(&conn).unwrap(), 0);
    }

    /// The guard. A write that lands while a job is embedding bumps the entry's `doc_hash`; the job's
    /// dequeue must then MISS, or the newer text is never indexed and the vector quietly describes
    /// text that is no longer in the row — a stale-vector bug with no symptom until a search misses.
    #[test]
    fn an_entry_that_moved_on_is_not_settled_by_the_job_that_claimed_it() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        let spec = owed(&conn, &embedder, "a", "alpha");
        let claimed = doc("a", "alpha", &spec);

        // A write lands after the claim: the row's text moved, and the enqueue bumped the hash.
        let newer = doc("a", "gamma", &spec);
        enqueue(&conn, "a", &newer, INTERACTIVE).unwrap();

        settle(&conn, "a", &claimed).unwrap();
        assert_eq!(pending(&conn).unwrap(), 1, "the newer text is still owed");
        assert_eq!(next(&conn).unwrap().unwrap().doc_hash, newer);

        // Settling with the hash the entry now carries does clear it.
        settle(&conn, "a", &newer).unwrap();
        assert_eq!(pending(&conn).unwrap(), 0);
    }

    #[test]
    fn an_entry_whose_row_is_gone_is_dropped_rather_than_left_haunting_the_queue() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        let spec = VecSpec::of(&embedder);
        enqueue(&conn, "ghost", "h", INTERACTIVE).unwrap();

        assert!(!encode_one(&conn, &embedder, &spec, "ghost").unwrap());
        assert_eq!(pending(&conn).unwrap(), 0, "nothing to embed, nothing owed");
    }

    #[test]
    fn drain_empties_the_queue_and_reports_how_much_it_did() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        let spec = owed(&conn, &embedder, "a", "alpha");
        owed(&conn, &embedder, "b", "beta");
        owed(&conn, &embedder, "c", "gamma");
        let rows = [("a", "alpha"), ("b", "beta"), ("c", "gamma")];
        for (key, body) in rows {
            enqueue(&conn, key, &doc(key, body, &spec), INTERACTIVE).unwrap();
        }

        assert_eq!(drain(&conn, &embedder, &spec, 10).unwrap(), 3);
        assert_eq!(pending(&conn).unwrap(), 0);
        assert_eq!(failed(&conn).unwrap(), 0);
        for (key, body) in rows {
            assert_eq!(stored_hash(&conn, key), doc(key, body, &spec));
            assert!(
                index::vec::get(&conn, id_of(&conn, key)).unwrap().is_some(),
                "{key} is searchable"
            );
        }
        // A drain on an empty queue is a no-op, not an error.
        assert_eq!(drain(&conn, &embedder, &spec, 10).unwrap(), 0);
    }

    /// The two halves are the worker's contract: a caller holding the store lock claims, drops the
    /// lock to embed, and settles afterwards. That only works if the halves carry everything across,
    /// so the shape is pinned here.
    #[test]
    fn claim_and_settle_carry_a_job_across_the_embed() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        let spec = owed(&conn, &embedder, "a", "alpha");
        enqueue(&conn, "a", &doc("a", "alpha", &spec), INTERACTIVE).unwrap();

        let claimed = claim(&conn, &spec, "a").unwrap().expect("a claim");
        assert!(claimed.needs_embed(), "the row owes a vector");
        let vector = embedder.embed_one(claimed.doc()).unwrap();
        settle_encoded(&conn, &claimed, Some(&vector)).unwrap();
        assert_eq!(pending(&conn).unwrap(), 0);
        assert_eq!(stored_hash(&conn, "a"), doc("a", "alpha", &spec));

        // A claim on a row that is already indexed needs no model call — a re-enqueue whose text did
        // not move — and settling it with `None` must still clear the entry.
        enqueue(&conn, "a", &doc("a", "alpha", &spec), INTERACTIVE).unwrap();
        let again = claim(&conn, &spec, "a").unwrap().expect("a claim");
        assert!(
            !again.needs_embed(),
            "the vector already describes this text"
        );
        settle_encoded(&conn, &again, None).unwrap();
        assert_eq!(pending(&conn).unwrap(), 0);

        // But a job that DOES need a vector, settled with `None`, is a caller bug — not a silent
        // stale vector, which is what it would be if the half just wrote nothing.
        conn.execute("UPDATE memory SET doc_hash = '' WHERE key = 'a'", [])
            .unwrap();
        enqueue(&conn, "a", &doc("a", "alpha", &spec), INTERACTIVE).unwrap();
        let owed = claim(&conn, &spec, "a").unwrap().expect("a claim");
        assert!(owed.needs_embed());
        assert!(
            settle_encoded(&conn, &owed, None).is_err(),
            "the caller must supply the vector it claimed for"
        );
    }

    /// A document that cannot be embedded must not wedge the queue: the attempt is counted, the entry
    /// is left for a retry, and past `MAX_ATTEMPTS` it is skipped and reported rather than retried in
    /// a hot loop forever.
    #[test]
    fn a_failing_document_is_retried_then_skipped_and_reported() {
        /// Always fails, so the queue's failure path is what is under test.
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
        let spec = owed(&conn, &real, "a", "alpha");
        enqueue(&conn, "a", &doc("a", "alpha", &spec), INTERACTIVE).unwrap();

        for attempt in 1..=MAX_ATTEMPTS {
            // One entry per pass, which is the retry shape the worker has: one attempt per pass, and
            // the sleep between passes is the backoff. A wide `drain` would burn all the attempts in a
            // single pass — also bounded, but not the shape that backs off.
            assert_eq!(
                drain(&conn, &Broken, &spec, 1).unwrap(),
                1,
                "attempt {attempt}"
            );
            let counted: i64 = conn
                .query_row(
                    "SELECT attempts FROM encode_queue WHERE key = 'a'",
                    [],
                    |r| r.get(0),
                )
                .expect("still on the books");
            assert_eq!(counted, attempt, "the attempt was counted");
        }
        assert!(
            next(&conn).unwrap().is_none(),
            "an entry past MAX_ATTEMPTS is skipped, so the loop cannot wedge on it"
        );
        assert_eq!(failed(&conn).unwrap(), 1, "and it is reported, not silent");
        assert_eq!(pending(&conn).unwrap(), 1, "it is still on the books");

        // A good embedder still fixes it, so the skip is not a death sentence.
        enqueue(&conn, "a", &doc("a", "alpha", &spec), INTERACTIVE).unwrap();
        assert_eq!(drain(&conn, &real, &spec, 10).unwrap(), 1);
        assert_eq!(pending(&conn).unwrap(), 0);
        assert_eq!(
            failed(&conn).unwrap(),
            0,
            "a re-enqueue resets the attempts"
        );
    }
}
