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
    vec::upsert(&conn, a, &[1.0, 0.0, 0.0, 0.0]).unwrap();
    vec::upsert(&conn, b, &[0.9998, 0.02, 0.0, 0.0]).unwrap();
    vec::upsert(&conn, c, &[0.0, 1.0, 0.0, 0.0]).unwrap();

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
    vec::upsert(&conn, a, &[1.0, 0.0, 0.0, 0.0]).unwrap();
    vec::upsert(&conn, b, &[1.0, 0.0, 0.0, 0.0]).unwrap();

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
