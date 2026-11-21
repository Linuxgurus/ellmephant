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

    common::add_now(
        &conn,
        &embedder,
        &memory("v1", "First", "the original body"),
    )?;
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 0);

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

    let patch = Patch {
        key: Some("v1".into()),
        tags: Some(vec!["filed".into()]),
        ..Default::default()
    };
    common::merge_now(&conn, &embedder, &patch)?;
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 1);

    common::add_now(
        &conn,
        &embedder,
        &memory("v1", "First", "the second correction"),
    )?;
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 2);
    let v2 = store::version::read(&conn, "v1", 2)?.expect("version 2");
    assert_eq!(v2["body"], "the corrected body");

    assert_eq!(v2["key"], "v1");
    assert_eq!(v2["title"], "First");
    assert!(v2["tags"].is_array());
    assert!(v2["created"].as_str().is_some());

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

    assert!(store::delete(&conn, "gone", None)?);
    assert_eq!(count(&conn, "SELECT count(*) FROM memory"), 0);

    assert_eq!(count(&conn, "SELECT count(*) FROM memory_version"), 2);
    let v = store::version::read(&conn, "gone", 1)?.expect("still there");
    assert_eq!(v["body"], "first");
    let last = store::version::read(&conn, "gone", 2)?.expect("the deleted state");
    assert_eq!(last["body"], "second");

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
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    common::add_now(&conn, &embedder, &memory("kept", "Kept", "first"))?;
    common::add_now(&conn, &embedder, &memory("kept", "Kept", "second"))?;
    common::add_now(&conn, &embedder, &memory("gone", "Gone", "first"))?;
    common::add_now(&conn, &embedder, &memory("gone", "Gone", "second"))?;
    assert!(store::delete(&conn, "gone", None)?);

    let idx = store::version::index(&conn)?;
    assert_eq!(idx.len(), 2, "two keys hold history");

    let gone = idx.iter().find(|i| i.key == "gone").expect("orphan listed");
    assert!(
        gone.orphan,
        "its row is gone, so the history is the only copy"
    );
    assert_eq!(gone.versions, 2, "the edit AND the delete's own snapshot");
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

    let note_only = Patch {
        key: Some("noted".into()),
        version_note: Some("a note with nothing to version".into()),
        ..Default::default()
    };
    common::merge_now(&conn, &embedder, &note_only)?;
    assert_eq!(store::version::history(&conn, "noted")?.len(), 1);
    Ok(())
}
