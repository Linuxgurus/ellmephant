mod common;

use common::temp_store;
use ellmephant::embed::HashEmbedder;
use ellmephant::store::{self, Memory, snapshot};
use ellmephant::{Result, db};

const DIM: usize = 8;

fn memory(key: &str, body: &str) -> Memory {
    Memory {
        key: key.into(),
        title: format!("Title {key}"),
        body: body.into(),
        ..Default::default()
    }
}

#[test]
fn a_snapshot_is_a_usable_copy_and_not_a_link() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let embedder = HashEmbedder::new(DIM);
    common::add_now(
        &conn,
        &embedder,
        &memory("before", "written before the snapshot"),
    )?;

    assert_eq!(snapshot::age_seconds(paths.root()), None, "no snapshot yet");

    let reader = db::open_read_only(&paths)?;
    let out = snapshot::write(&reader, paths.root())?;
    assert!(out.exists());
    assert_eq!(out, snapshot::file(paths.root()));

    common::add_now(
        &conn,
        &embedder,
        &memory("after", "written after the snapshot"),
    )?;

    let copy = rusqlite::Connection::open(&out)?;
    let version: i32 = copy.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    assert_eq!(
        version,
        db::migrate::latest(),
        "the copy carries the same schema as the live store"
    );
    let in_copy: i64 = copy.query_row(
        "SELECT count(*) FROM memory WHERE key = 'before'",
        [],
        |r| r.get(0),
    )?;
    let leaked: i64 =
        copy.query_row("SELECT count(*) FROM memory WHERE key = 'after'", [], |r| {
            r.get(0)
        })?;
    assert_eq!(in_copy, 1, "the state at snapshot time is there");
    assert_eq!(
        leaked, 0,
        "a later write is NOT in the copy — it is a point in time"
    );

    assert!(store::get(&conn, "after")?.is_some());
    assert!(snapshot::age_seconds(paths.root()).is_some_and(|s| s < 60));
    Ok(())
}

#[test]
fn a_second_snapshot_replaces_the_first_atomically() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let embedder = HashEmbedder::new(DIM);

    common::add_now(&conn, &embedder, &memory("one", "first"))?;
    snapshot::write(&conn, paths.root())?;

    common::add_now(&conn, &embedder, &memory("two", "second"))?;
    let out = snapshot::write(&conn, paths.root())?;

    let copy = rusqlite::Connection::open(&out)?;
    let n: i64 = copy.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
    assert_eq!(n, 2);

    let leftovers: Vec<_> = std::fs::read_dir(snapshot::dir(paths.root()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    Ok(())
}
