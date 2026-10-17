//! Integration: the CAS token — `expected_version`, so a read-modify-write cannot lose an update.
//!
//! The store versions a row but does not transact, so two writers can both read `$0.95`, both write
//! `$0.75`, and last-write-wins silently drops one debit. The claim serializes FINGERS; this makes
//! the WRITE itself safe, which is what a spender outside a finger needs.
//!
//! Runs offline against a real store file, with the deterministic `HashEmbedder`.

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

/// A CREATE needs a title as well as a body, so it gets its own constructor.
fn create(key: &str, body: &str, expected: Option<i64>) -> Patch {
    Patch {
        title: Some("Fresh".into()),
        ..patch(key, body, expected)
    }
}

/// The token is the LIVE state's number: a missing key has none (`0`), a fresh row is `1`, and every
/// superseded state bumps it by one.
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

/// The whole point: a token that no longer matches writes NOTHING, and says what the current version
/// is so the caller can re-read and retry. A lost update is now a visible conflict.
#[test]
fn a_stale_token_writes_nothing_and_reports_the_current_version() -> Result<()> {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths)?;
    let embedder = HashEmbedder::new(DIM);
    common::add_now(&conn, &embedder, &memory("bank", "Bank", "balance 10"))?;

    // Two writers read version 1. The first commits, taking the key to version 2.
    let first = merge_now(&conn, &embedder, &patch("bank", "balance 9", Some(1)))?;
    assert_eq!(first.conflict, None, "the first writer wins");
    assert!(!first.created);

    // The second still believes version 1, so its write is refused rather than clobbering the first.
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

    // With the fresh token the same write succeeds.
    let retried = merge_now(&conn, &embedder, &patch("bank", "balance 8", Some(2)))?;
    assert_eq!(retried.conflict, None);
    assert_eq!(store::live_version(&conn, "bank")?, 3);
    Ok(())
}

/// `0` means "this key does not exist yet", so a CREATE can be raced too: two creators cannot both
/// believe they wrote the first state.
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

/// ★ THE RACE ITSELF, with real connections and a starting barrier: N writers hold the SAME token, so
/// exactly one may commit and every other must come back with a conflict. This is the bank bug.
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
            // Everyone reads the SAME version, then starts together.
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

    // And the row is the winner's, at the next version — not a torn mixture.
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

/// An omitted token keeps last-write-wins, so every existing caller is unchanged.
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
