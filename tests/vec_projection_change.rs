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

    {
        let conn = open(&paths);
        meta::set(&conn, meta::PROJECTION, "a-recipe-from-before").expect("set");
    }

    let conn = open(&paths);

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

    assert!(
        !vec::projection_stale(&conn).expect("stale"),
        "the fingerprint is recorded with the purge, not after it"
    );

    let hint = vec::Knn {
        k: 10,
        max_distance: None,
    };
    let hits = vec::knn(&conn, &[0.5; 8], &hint, None).expect("knn");
    assert!(
        hits.is_empty(),
        "an unfinished rebuild returns no hits, not wrong ones"
    );

    common::settle(&conn, &embedder);
    assert_eq!(vec::count(&conn).expect("count"), 3);
    assert_eq!(encode::pending(&conn).expect("pending"), 0);
}

#[test]
fn an_unrecorded_projection_with_vectors_is_rebuilt_not_assumed_current() {
    let (_dir, paths) = temp_store();
    let embedder = HashEmbedder::new(8);
    seed(&paths, &embedder, &["alpha", "beta"]);

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

    let conn = open(&paths);
    assert_eq!(vec::count(&conn).expect("count"), 2);
    assert_eq!(encode::pending(&conn).expect("pending"), 0);
    assert!(!vec::projection_stale(&conn).expect("stale"));
}

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

    let mut cfg = ellmephant::config::Config {
        rebuild_max_documents: 2,
        ..Default::default()
    };
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
