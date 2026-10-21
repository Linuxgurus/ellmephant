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

/// ★ soma/ellmephant#76: the purge is milliseconds and the rebuild is hours. A config change must not
/// commit the store to work nobody priced. The refusal is the INDEX-ALREADY-HAS-A-NAME-FOR-THIS state:
/// STALE, with the old vectors still answering and a decision recorded on the books.
#[test]
fn a_rebuild_that_exceeds_the_budget_is_refused_and_nothing_is_purged() {
    use ellmephant::store::{links::Kind, maintain};

    let (_dir, paths) = temp_store();
    let embedder = HashEmbedder::new(8);
    seed(&paths, &embedder, &["alpha", "beta", "gamma"]);
    {
        let conn = open(&paths);
        meta::set(&conn, meta::PROJECTION, "a-recipe-from-before").expect("set");
    }

    // Three documents, ceiling of two: unaffordable by this store's own arithmetic.
    let mut cfg = ellmephant::config::Config::default();
    cfg.rebuild_max_documents = 2;
    let conn = db::open_with(&paths, &cfg).expect("open");

    assert_eq!(
        vec::count(&conn).expect("count"),
        3,
        "NOTHING was purged — the old index keeps answering"
    );
    assert!(
        vec::projection_stale(&conn).expect("stale"),
        "and it says STALE, which is the honest word for it"
    );
    assert_eq!(
        encode::pending(&conn).expect("pending"),
        0,
        "no queue nobody agreed to pay for"
    );

    let record = meta::get(&conn, maintain::REBUILD)
        .expect("read")
        .expect("the decision is on the books");
    assert!(record.starts_with("REFUSED"), "{record}");
    assert!(record.contains("docs=3"), "the estimate is named: {record}");
    assert!(record.contains("budget=2"), "{record}");
    assert!(
        record.contains("est_s=35"),
        "3 x 11.5s = 34.5, rounded up: {record}"
    );

    // ★ AND THE WALL: a write cannot land a new-model vector beside the old ones. The entry stays
    // queued (visible debt) and the settle refuses.
    let writ = Memory {
        key: "delta".to_string(),
        title: "delta".to_string(),
        body: "written while the index is stale".to_string(),
        ..Default::default()
    };
    ellmephant::store::add(&conn, &writ).expect("the write itself is allowed");
    let err = encode::encode_one(&conn, &embedder, &Kind::Memory.keyslug("delta"))
        .expect_err("a stale index must not accept a vector");
    assert!(err.to_string().contains("STALE"), "{err}");
    assert_eq!(
        encode::pending(&conn).expect("pending"),
        1,
        "the vector is still OWED, visibly, rather than silently mixed"
    );

    // The deliberate override is a config change, and it proceeds.
    cfg.rebuild_allow_over_budget = true;
    let conn = db::open_with(&paths, &cfg).expect("open");
    assert_eq!(vec::count(&conn).expect("count"), 0, "purged");
    assert!(encode::pending(&conn).expect("pending") >= 3, "and queued");
    assert!(
        !vec::projection_stale(&conn).expect("stale"),
        "the fingerprint moved WITH the purge"
    );
    let record = meta::get(&conn, maintain::REBUILD)
        .expect("read")
        .expect("the start is on the books");
    assert!(record.contains("started="), "{record}");
}
