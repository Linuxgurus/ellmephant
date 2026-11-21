use rusqlite::{Connection, OptionalExtension, params};

use crate::embed::{Embedder, projection};
use crate::error::{Error, Result};
use crate::index;
use crate::store::links::Kind;

pub const PRIORITY_LIVE: i32 = 3;

pub const PRIORITY_BULK: i32 = 5;

pub const SCHEMA: i32 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub keyslug: String,
    pub content: String,
    pub priority: i32,
}

pub fn enqueue(conn: &Connection, keyslug: &str, content: &str, priority: i32) -> Result<()> {
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

pub fn forget(conn: &Connection, keyslug: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM encode_queue WHERE keyslug = ?1",
        params![keyslug],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Claim {
    keyslug: String,
    id: i64,
    doc: String,
    claimed_content: String,
}

impl Claim {
    pub fn doc(&self) -> &str {
        &self.doc
    }

    pub fn keyslug(&self) -> &str {
        &self.keyslug
    }
}

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
        forget(conn, keyslug)?;
        return Ok(None);
    };
    let id: Option<i64> = match kind {
        Kind::Memory => conn
            .query_row("SELECT id FROM memory WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?,
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

pub fn settle_encoded(conn: &Connection, claim: &Claim, vector: &[f32]) -> Result<()> {
    if index::vec::projection_stale(conn)? {
        return Err(Error::Config(
            "the vector index is STALE and its rebuild did not run or was refused — refusing to write \
             a vector that would mix models. Raise `rebuild_max_documents`, roll the config back, or \
             run `memory_reembed`."
                .into(),
        ));
    }
    index::vec::ensure(conn, vector.len())?;
    let tx = conn.unchecked_transaction()?;
    let (kind, key) = Kind::from_keyslug(&claim.keyslug).ok_or_else(|| {
        Error::Config(format!(
            "claim has an unaddressable keyslug: {}",
            claim.keyslug
        ))
    })?;
    index::vec::upsert(&tx, claim.id, kind.as_str(), key, vector)?;
    settle(&tx, &claim.keyslug, &claim.claimed_content)?;
    tx.commit()?;
    Ok(())
}

pub fn encode_one(conn: &Connection, embedder: &dyn Embedder, keyslug: &str) -> Result<bool> {
    let Some(claimed) = claim(conn, keyslug)? else {
        return Ok(false);
    };
    let vector = embedder.embed_one(claimed.doc())?;
    settle_encoded(conn, &claimed, &vector)?;
    Ok(true)
}

fn settle(conn: &Connection, keyslug: &str, claimed_content: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM encode_queue WHERE keyslug = ?1 AND content = ?2",
        params![keyslug, claimed_content],
    )?;
    Ok(())
}

pub fn drain(conn: &Connection, embedder: &dyn Embedder, limit: usize) -> Result<usize> {
    let mut done = 0;
    let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();
    while done < limit {
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

pub fn missing(conn: &Connection) -> Result<Vec<(String, String)>> {
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub pending: i64,
    pub failed: i64,
    pub oldest_s: Option<i64>,
}

pub fn is_embeddable(keyslug: &str) -> bool {
    match Kind::from_keyslug(keyslug) {
        Some((kind, _)) => kind.is_embeddable(),
        None => true,
    }
}

pub fn owed(conn: &Connection) -> Result<i64> {
    let mut stmt = conn.prepare("SELECT keyslug FROM encode_queue")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows
        .filter_map(|r| r.ok())
        .filter(|slug| is_embeddable(slug))
        .count() as i64)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    pub stale: bool,
    pub owed: i64,
    pub recorded: bool,
}

impl Projection {
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

pub fn projection(conn: &Connection) -> Result<Projection> {
    Ok(Projection {
        stale: index::vec::projection_stale(conn)?,
        owed: owed(conn)?,
        recorded: crate::db::meta::get(conn, crate::db::meta::PROJECTION)?.is_some(),
    })
}

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

    fn none() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    fn queue_owed(conn: &Connection, embedder: &dyn Embedder, key: &str, body: &str) {
        let _ = embedder;
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

    #[test]
    fn an_entry_that_moved_on_is_not_settled_by_the_job_that_claimed_it() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);
        queue_owed(&conn, &embedder, "a", "alpha");
        let claimed = claim(&conn, &slug("a")).unwrap().expect("a claim");
        let claimed_content = claimed.doc().to_string();

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

    #[test]
    fn a_kind_that_owes_no_vector_is_neither_queued_nor_owed() {
        let (_d, conn) = store();
        conn.execute(
            "INSERT INTO encode_queue (keyslug, content, priority) VALUES ('project:sessions', 'x', 3)",
            [],
        )
        .unwrap();
        assert_eq!(pending(&conn).unwrap(), 1, "it is in the queue, as a row");
        assert_eq!(owed(&conn).unwrap(), 0, "but it owes nothing");

        enqueue(&conn, "project:sessions", "y", PRIORITY_LIVE).unwrap();
        assert_eq!(pending(&conn).unwrap(), 1, "still one row, and unchanged");

        enqueue(&conn, &slug("a"), "doc", PRIORITY_LIVE).unwrap();
        assert_eq!(owed(&conn).unwrap(), 1, "a memory IS owed");

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
        assert_eq!(drain(&conn, &embedder, 10).unwrap(), 0);
    }

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

        crate::store::add(&conn, &mem("a", "alpha, revised")).unwrap();
        assert_eq!(drain(&conn, &Broken, 10).unwrap(), 1);

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

        assert_eq!(drain(&conn, &real, 10).unwrap(), 1);
        assert_eq!(pending(&conn).unwrap(), 0);
        assert_ne!(index::vec::get(&conn, id).unwrap().unwrap(), current);
    }

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

    #[test]
    fn missing_names_the_documents_that_have_no_vector() {
        let (_d, conn) = store();
        let embedder = HashEmbedder::new(8);

        crate::store::add(&conn, &mem("a", "alpha")).unwrap();
        assert_eq!(
            missing(&conn).unwrap().len(),
            1,
            "written and not embedded is missing, index or no index"
        );

        drain(&conn, &embedder, 10).unwrap();
        assert_eq!(missing(&conn).unwrap().len(), 0, "all indexed");

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
    }
}
