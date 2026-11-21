mod common;

use std::sync::{Arc, Barrier};

use common::{merge_now, temp_store};
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

fn patch(key: &str, body: &str, expected: Option<i64>) -> Patch {
    Patch {
        key: Some(key.into()),
        body: Some(body.into()),
        expected_version: expected,
        ..Default::default()
    }
}

fn create(key: &str, body: &str, expected: Option<i64>) -> Patch {
    Patch {
        title: Some("Fresh".into()),
        ..patch(key, body, expected)
    }
}

#[test]
fn the_version_counts_live_states() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let embedder = HashEmbedder::new(DIM);

    assert_eq!(store::live_version(&conn, "cas")?, 0, "no row, no version");
    assert!(store::get(&conn, "cas")?.is_none());

    common::add_now(&conn, &embedder, &memory("cas", "Balance", "spent 0"))?;
    assert_eq!(store::live_version(&conn, "cas")?, 1);
    assert_eq!(
        store::get(&conn, "cas")?.expect("row").version,
        Some(1),
        "a reader is handed the token it must pass back"
    );

    merge_now(&conn, &embedder, &patch("cas", "spent 1", None))?;
    assert_eq!(store::live_version(&conn, "cas")?, 2);
    assert_eq!(store::get(&conn, "cas")?.expect("row").version, Some(2));
    Ok(())
}

#[test]
fn a_stale_token_writes_nothing_and_reports_the_current_version() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let embedder = HashEmbedder::new(DIM);
    common::add_now(&conn, &embedder, &memory("bank", "Bank", "balance 10"))?;

    let first = merge_now(&conn, &embedder, &patch("bank", "balance 9", Some(1)))?;
    assert_eq!(first.conflict, None, "the first writer wins");
    assert!(!first.created);

    let stale = merge_now(&conn, &embedder, &patch("bank", "balance 8", Some(1)))?;
    assert_eq!(
        stale.conflict,
        Some(2),
        "refused, and told where the row is now"
    );
    assert!(!stale.created, "a refused write created nothing");
    assert_eq!(
        store::get(&conn, "bank")?.expect("row").body,
        "balance 9",
        "the loser's body never landed"
    );

    let retried = merge_now(&conn, &embedder, &patch("bank", "balance 8", Some(2)))?;
    assert_eq!(retried.conflict, None);
    assert_eq!(store::live_version(&conn, "bank")?, 3);
    Ok(())
}

#[test]
fn a_create_is_guarded_by_version_zero() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let embedder = HashEmbedder::new(DIM);

    let created = merge_now(&conn, &embedder, &create("fresh", "one", Some(0)))?;
    assert_eq!(created.conflict, None);
    assert!(created.created);

    let second = merge_now(&conn, &embedder, &create("fresh", "two", Some(0)))?;
    assert_eq!(second.conflict, Some(1), "no row at version 0 any more");
    assert_eq!(store::get(&conn, "fresh")?.expect("row").body, "one");
    Ok(())
}

#[test]
fn a_race_has_exactly_one_winner() -> Result<()> {
    const WRITERS: usize = 8;

    let (_dir, paths) = temp_store();
    {
        let conn = db::open(&paths)?;
        let embedder = HashEmbedder::new(DIM);
        common::add_now(&conn, &embedder, &memory("race", "Race", "start"))?;
    }

    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut handles = Vec::new();
    for i in 0..WRITERS {
        let paths = paths.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let conn = db::open(&paths).expect("open");
            let embedder = HashEmbedder::new(DIM);
            let seen = store::get(&conn, "race")
                .expect("get")
                .expect("row")
                .version;
            barrier.wait();
            let out = merge_now(
                &conn,
                &embedder,
                &patch("race", &format!("writer {i}"), seen),
            )
            .expect("merge");
            out.conflict.is_none()
        }));
    }
    let wins = handles
        .into_iter()
        .map(|h| h.join().expect("thread"))
        .filter(|won| *won)
        .count();
    assert_eq!(wins, 1, "exactly one writer may commit a CASed row");

    let conn = db::open(&paths)?;
    assert_eq!(store::live_version(&conn, "race")?, 2);
    assert!(
        store::get(&conn, "race")?
            .expect("row")
            .body
            .starts_with("writer ")
    );
    Ok(())
}

#[test]
fn no_token_still_writes() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let embedder = HashEmbedder::new(DIM);
    common::add_now(&conn, &embedder, &memory("plain", "T", "one"))?;
    let out = merge_now(&conn, &embedder, &patch("plain", "two", None))?;
    assert_eq!(out.conflict, None);
    assert_eq!(store::get(&conn, "plain")?.expect("row").body, "two");
    Ok(())
}
