//! Integration: the duplicate-candidate audit (`memory_dupes`) against real `vec0` vectors.
//!
//! Runs offline. Vectors are written directly so the distances are exact — the finder is about
//! geometry, not the embedder.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::index::vec;
use ellmephant::store::{Memory, dupes};

/// A memory with an explicit key.
fn memory(key: &str, title: &str, body: &str) -> Memory {
    Memory {
        key: key.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

#[test]
fn finds_only_the_pair_inside_the_ceiling() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);

    let (a, _) = common::add_now(&conn, &embedder, &memory("alpha", "A", "body a")).unwrap();
    let (b, _) = common::add_now(&conn, &embedder, &memory("beta", "B", "body b")).unwrap();
    let (c, _) = common::add_now(&conn, &embedder, &memory("gamma", "C", "body c")).unwrap();

    // Controlled vectors: alpha ≈ beta; gamma orthogonal to both.
    vec::upsert(&conn, a, "memory", "ka", &[1.0, 0.0, 0.0, 0.0]).unwrap();
    vec::upsert(&conn, b, "memory", "kb", &[0.9998, 0.02, 0.0, 0.0]).unwrap();
    vec::upsert(&conn, c, "memory", "kc", &[0.0, 1.0, 0.0, 0.0]).unwrap();

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs.len(), 1, "{pairs:?}");
    let p = &pairs[0];
    assert_eq!((p.key_a.as_str(), p.key_b.as_str()), ("alpha", "beta"));
    assert_eq!((p.title_a.as_str(), p.title_b.as_str()), ("A", "B"));
    assert!(p.distance < 0.01, "closer than the ceiling: {}", p.distance);

    // A ceiling below the actual distance finds nothing...
    assert!(dupes::find(&conn, 0.0001, 50).unwrap().is_empty());
    // ...and `limit` caps the list.
    assert!(dupes::find(&conn, 0.05, 0).unwrap().is_empty());
}

#[test]
fn a_pair_is_emitted_once_not_twice() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    let (a, _) = common::add_now(&conn, &embedder, &memory("one", "One", "x")).unwrap();
    let (b, _) = common::add_now(&conn, &embedder, &memory("two", "Two", "y")).unwrap();
    // Identical vectors: distance 0, and the pair must not appear from both ends.
    vec::upsert(&conn, a, "memory", "ka", &[1.0, 0.0, 0.0, 0.0]).unwrap();
    vec::upsert(&conn, b, "memory", "kb", &[1.0, 0.0, 0.0, 0.0]).unwrap();

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs.len(), 1, "one pair, not two: {pairs:?}");
    assert_eq!(pairs[0].distance, 0.0);
}

#[test]
fn an_empty_or_single_row_store_is_quiet() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    assert!(dupes::find(&conn, 0.05, 50).unwrap().is_empty());

    let embedder = HashEmbedder::new(4);
    common::add_now(&conn, &embedder, &memory("solo", "Solo", "only")).unwrap();
    assert!(dupes::find(&conn, 0.05, 50).unwrap().is_empty());
}

/// A pair of rows with controlled, identical vectors: distance 0, so the class is the only thing
/// under test.
fn pair(
    conn: &rusqlite::Connection,
    embedder: &HashEmbedder,
    key_a: &str,
    body_a: &str,
    key_b: &str,
    body_b: &str,
) {
    let (a, _) = common::add_now(conn, embedder, &memory(key_a, key_a, body_a)).unwrap();
    let (b, _) = common::add_now(conn, embedder, &memory(key_b, key_b, body_b)).unwrap();
    vec::upsert(conn, a, "memory", key_a, &[1.0, 0.0, 0.0, 0.0]).unwrap();
    vec::upsert(conn, b, "memory", key_b, &[1.0, 0.0, 0.0, 0.0]).unwrap();
}

#[test]
fn a_dated_series_is_marked_series_and_the_plan_refuses_it() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    // The same measurement on two days is near-identical prose — that is exactly why the class
    // cannot come from distance.
    let body = "offsite backup verified: 606 ids, 0 unreconstructable.";
    pair(
        &conn,
        &embedder,
        "offsite-2026-11-05",
        body,
        "offsite-2026-11-06",
        body,
    );

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs.len(), 1, "{pairs:?}");
    assert_eq!(pairs[0].class, dupes::Class::Series);

    let plan = dupes::plan(&conn, &pairs).unwrap();
    assert!(
        plan.steps.is_empty(),
        "a series is never a merge step: {plan:?}"
    );
    assert_eq!(plan.refused.len(), 1);
    assert_eq!(plan.refused[0].class, dupes::Class::Series);
    assert!(plan.refused[0].reason.contains("dated series"), "{plan:?}");
}

#[test]
fn the_same_row_twice_is_a_duplicate_and_the_plan_names_the_keeper() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    let body = "the turn boundary is offered once per wake, and a miss is not a failure.";
    pair(
        &conn,
        &embedder,
        "aaa-spine-boundary",
        body,
        "bbb-spine-boundary",
        body,
    );

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs[0].class, dupes::Class::Duplicate);

    let plan = dupes::plan(&conn, &pairs).unwrap();
    assert_eq!(plan.steps.len(), 1, "{plan:?}");
    assert_eq!(plan.steps[0].keep, "aaa-spine-boundary");
    assert_eq!(plan.steps[0].absorb, "bbb-spine-boundary");
    assert!(!plan.steps[0].reason.is_empty(), "a plan must say why");
}

#[test]
fn a_verbatim_capture_with_an_address_is_an_artifact() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    let body = "diff --git a/src/x.rs b/src/x.rs\n@@ -1,2 +1,3 @@\n-old\n+new\ncommit 643eeb8";
    pair(
        &conn,
        &embedder,
        "pr192-current-diff",
        body,
        "pr192-head-diff",
        body,
    );

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs[0].class, dupes::Class::Artifact);
    let plan = dupes::plan(&conn, &pairs).unwrap();
    assert!(
        plan.steps.is_empty(),
        "an artifact needs a cited survivor first: {plan:?}"
    );
}

#[test]
fn close_by_embedding_but_different_in_content_is_distinct() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    pair(
        &conn,
        &embedder,
        "offsite-retention",
        "backups are verified by opening a copy, never by the copy existing.",
        "offsite-schedule",
        "the offsite job runs at 04:30 and reports the id count it verified.",
    );

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs[0].class, dupes::Class::Distinct);
    let plan = dupes::plan(&conn, &pairs).unwrap();
    assert!(plan.steps.is_empty());
    assert_eq!(plan.refused[0].class, dupes::Class::Distinct);
}
