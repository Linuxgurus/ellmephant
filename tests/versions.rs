//! Integration: memory VERSIONS — the state a document change replaces (migration `0011`).
//!
//! Runs offline against a real store file, with the deterministic `HashEmbedder`.

mod common;

use common::{count, temp_store};
use ellmephant::embed::HashEmbedder;
use ellmephant::store::{self, Memory, Patch};
use ellmephant::{Result, db};

const DIM: usize = 8;

fn memory(key: &str, title: &str, body: &str) -> Memory {
    Memory {
        key: key.into(),
        title: title.into(),
        body: body.into(),
        ..Default::default()
    }
}

#[test]
fn a_document_change_snapshots_the_state_it_replaces() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    // A CREATE makes no version: nothing was replaced.
    common::add_now(
        &conn,
        &embedder,
        &memory("v1", "First", "the original body"),
    )?;
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 0);

    // A document change snapshots the ORIGINAL, and the live row carries the new text.
    common::add_now(
        &conn,
        &embedder,
        &memory("v1", "First", "the corrected body"),
    )?;
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 1);
    let hist = store::version::history(&conn, "v1")?;
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].n, 1);
    assert_eq!(hist[0].encoding, "json");
    assert!(hist[0].bytes > 0, "a version reports its size");
    let old = store::version::read(&conn, "v1", 1)?.expect("version 1");
    assert_eq!(old["body"], "the original body");
    assert_eq!(
        store::get(&conn, "v1")?.expect("live row").body,
        "the corrected body"
    );

    // A TAG-ONLY write is filing, not meaning: no version, because the document did not move.
    let patch = Patch {
        key: Some("v1".into()),
        tags: Some(vec!["filed".into()]),
        ..Default::default()
    };
    common::merge_now(&conn, &embedder, &patch)?;
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 1);

    // A second document change supersedes the CORRECTED text, so n=2 holds that state.
    common::add_now(
        &conn,
        &embedder,
        &memory("v1", "First", "the second correction"),
    )?;
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 2);
    let v2 = store::version::read(&conn, "v1", 2)?.expect("version 2");
    assert_eq!(v2["body"], "the corrected body");

    // The snapshot is self-describing: the fields the row held ride inside it.
    assert_eq!(v2["key"], "v1");
    assert_eq!(v2["title"], "First");
    assert!(v2["tags"].is_array());
    assert!(v2["created"].as_str().is_some());

    // A version that does not exist is honestly absent, not an error.
    assert!(store::version::read(&conn, "v1", 99)?.is_none());
    assert!(store::version::history(&conn, "no-such-key")?.is_empty());
    Ok(())
}

#[test]
fn history_survives_a_delete_so_erasure_is_a_policy() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    common::add_now(&conn, &embedder, &memory("gone", "Title", "first"))?;
    common::add_now(&conn, &embedder, &memory("gone", "Title", "second"))?;
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 1);

    assert!(store::delete(&conn, "gone")?);
    assert_eq!(count(&conn, "SELECT count(*) FROM memory"), 0);

    // No FK on the version table, so history OUTLIVES the row. That is deliberate: the evidence of
    // what a key said is worth more than the tidiness of a cascade.
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 2);
    let v = store::version::read(&conn, "gone", 1)?.expect("still there");
    assert_eq!(v["body"], "first");
    // AND the delete snapshotted the state it removed, so the newest version IS what the key said
    // when it died. Nothing is lost by a delete — only an address goes quiet.
    let last = store::version::read(&conn, "gone", 2)?.expect("the deleted state");
    assert_eq!(last["body"], "second");

    // Which makes erasure a POLICY rather than an accident: one act, and it is gone. This is now the
    // store's ONLY true destroyer, because every other write path keeps what it supersedes.
    assert_eq!(store::version::erase(&conn, "gone")?, 2);
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 0);
    assert_eq!(
        store::version::erase(&conn, "gone")?,
        0,
        "erasing twice is a no-op"
    );
    Ok(())
}

#[test]
fn the_index_finds_history_whose_row_is_gone() -> Result<()> {
    // A delete leaves an ADDRESS with history behind it. Nothing lists deleted keys, so the
    // question "where did it go?" needs its own answer: the index of history, with orphans marked.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    common::add_now(&conn, &embedder, &memory("kept", "Kept", "first"))?;
    common::add_now(&conn, &embedder, &memory("kept", "Kept", "second"))?;
    common::add_now(&conn, &embedder, &memory("gone", "Gone", "first"))?;
    common::add_now(&conn, &embedder, &memory("gone", "Gone", "second"))?;
    assert!(store::delete(&conn, "gone")?);

    let idx = store::version::index(&conn)?;
    assert_eq!(idx.len(), 2, "two keys hold history");

    let gone = idx.iter().find(|i| i.key == "gone").expect("orphan listed");
    assert!(
        gone.orphan,
        "its row is gone, so the history is the only copy"
    );
    assert_eq!(gone.versions, 2, "the edit AND the delete's own snapshot");
    // ...and it is still readable one version at a time, by ADDRESS.
    assert_eq!(
        store::version::read(&conn, "gone", 1)?.expect("version 1")["body"],
        "first"
    );

    let kept = idx
        .iter()
        .find(|i| i.key == "kept")
        .expect("live key listed");
    assert!(!kept.orphan);
    Ok(())
}

#[test]
fn a_version_carries_why_the_change_was_made() -> Result<()> {
    // The note is not a field of the memory: it is the INTENT of a write, recorded beside the version
    // that write supersedes, so a later reader learns why instead of reconstructing it from prose.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    common::add_now(&conn, &embedder, &memory("noted", "Noted", "first"))?;
    let why = "SPEC §7 changed: the old wording promised a guarantee the store does not make";
    let patch = Patch {
        key: Some("noted".into()),
        body: Some("second".into()),
        version_note: Some(why.into()),
        ..Default::default()
    };
    common::merge_now(&conn, &embedder, &patch)?;

    let hist = store::version::history(&conn, "noted")?;
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].note.as_deref(), Some(why));

    // A note ALONE is not a change: the document did not move, so there is nothing to version, and
    // the note is simply dropped rather than inventing a version with no state behind it.
    let note_only = Patch {
        key: Some("noted".into()),
        version_note: Some("a note with nothing to version".into()),
        ..Default::default()
    };
    common::merge_now(&conn, &embedder, &note_only)?;
    assert_eq!(store::version::history(&conn, "noted")?.len(), 1);
    Ok(())
}
