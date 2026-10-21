//! A changed (or unprovable) projection is a PURGE AND FULL REBUILD, never a refill.
//!
//! The failure this guards, reported by stet (2026-10-21) and reachable in one write:
//!
//! 1. the model changes and `meta.PROJECTION` moves;
//! 2. a write lands before the rebuild — new text, so it enqueues and is embedded with the NEW model
//!    beside old-model vectors;
//! 3. a refill asks "what has no vector" and **skips that row, because it has one**.
//!
//! Result: a mixed index, an empty queue, and an invariant reporting "current". Distances across two
//! models are meaningless, so the index ranks garbage confidently — worse than an empty one.

mod common;

use common::temp_store;
use ellmephant::db::{self, meta};
use ellmephant::embed::{Embedder, HashEmbedder, VecSpec};
use ellmephant::index::vec;
use ellmephant::store::{Memory, encode};

fn memory(key: &str, body: &str) -> Memory {
    Memory {
        key: key.to_string(),
        title: key.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

fn open(paths: &ellmephant::config::Paths) -> rusqlite::Connection {
    db::open(paths).expect("open")
}

fn seed(paths: &ellmephant::config::Paths, embedder: &dyn Embedder, keys: &[&str]) {
    let conn = open(paths);
    let _spec = VecSpec::of(embedder);
    for key in keys {
        common::add_now(&conn, embedder, &memory(key, &format!("body of {key}"))).expect("add");
    }
    assert_eq!(vec::count(&conn).expect("count"), keys.len() as i64);
    assert_eq!(
        encode::pending(&conn).expect("pending"),
        0,
        "the queue is drained"
    );
}

#[test]
fn a_changed_projection_purges_and_requeues_every_document() {
    let (_dir, paths) = temp_store();
    let embedder = HashEmbedder::new(8);
    seed(&paths, &embedder, &["alpha", "beta", "gamma"]);

    // The model changed: the stored fingerprint no longer matches the code.
    {
        let conn = open(&paths);
        meta::set(&conn, meta::PROJECTION, "a-recipe-from-before").expect("set");
    }

    let conn = open(&paths);

    // ★ NOT ONE OLD VECTOR SURVIVES — a write can no longer land a new-model vector beside them.
    assert_eq!(
        vec::count(&conn).expect("count"),
        0,
        "purged, tombstones included"
    );
    assert_eq!(vec::count_deleted(&conn).expect("deleted"), 0);
    assert_eq!(
        encode::pending(&conn).expect("pending"),
        3,
        "every document re-queued"
    );

    // The fingerprint moves in the SAME transaction as the purge, so "stale" cannot linger and be
    // mistaken for "nothing to do"; what says work is outstanding is the queue.
    assert!(
        !vec::projection_stale(&conn).expect("stale"),
        "the fingerprint is recorded with the purge, not after it"
    );

    // And the index answers honestly meanwhile: nothing, rather than old-model hits.
    let hint = vec::Knn {
        k: 10,
        max_distance: None,
    };
    let hits = vec::knn(&conn, &[0.5; 8], &hint, None).expect("knn");
    assert!(
        hits.is_empty(),
        "an unfinished rebuild returns no hits, not wrong ones"
    );

    // Settle: the rebuild completes and the index is whole again.
    common::settle(&conn, &embedder);
    assert_eq!(vec::count(&conn).expect("count"), 3);
    assert_eq!(encode::pending(&conn).expect("pending"), 0);
}

#[test]
fn an_unrecorded_projection_with_vectors_is_rebuilt_not_assumed_current() {
    let (_dir, paths) = temp_store();
    let embedder = HashEmbedder::new(8);
    seed(&paths, &embedder, &["alpha", "beta"]);

    // A store written before projections were fingerprinted: vectors, no fingerprint. That is
    // "cannot be proven", and stamping it current would be the silent lie `projection_stale` refuses.
    {
        let conn = open(&paths);
        conn.execute("DELETE FROM meta WHERE k = ?1", [meta::PROJECTION])
            .expect("unrecord");
    }

    let conn = open(&paths);
    assert_eq!(
        vec::count(&conn).expect("count"),
        0,
        "unprovable vectors are purged"
    );
    assert_eq!(encode::pending(&conn).expect("pending"), 2);

    common::settle(&conn, &embedder);
    assert_eq!(vec::count(&conn).expect("count"), 2);
    assert!(meta::get(&conn, meta::PROJECTION).expect("get").is_some());
}

#[test]
fn a_healthy_store_is_left_alone_by_the_open_path() {
    let (_dir, paths) = temp_store();
    let embedder = HashEmbedder::new(8);
    seed(&paths, &embedder, &["alpha", "beta"]);

    // Re-opening must not purge, re-queue or otherwise churn a store whose projection is current.
    let conn = open(&paths);
    assert_eq!(vec::count(&conn).expect("count"), 2);
    assert_eq!(encode::pending(&conn).expect("pending"), 0);
    assert!(!vec::projection_stale(&conn).expect("stale"));
}
