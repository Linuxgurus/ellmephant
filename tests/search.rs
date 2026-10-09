//! Integration: the P5 read path against a real store file and real `vec0` tables.
//!
//! Runs offline — the deterministic [`HashEmbedder`] stands in for Qwen3. It is a
//! bag-of-words model, so "meaning" here reduces to shared tokens; that is enough to prove
//! the constraints run and hydrate without loading weights.

mod common;

use common::temp_store;
use llmphant::db;
use llmphant::embed::HashEmbedder;
use llmphant::search::{self, Query};
use llmphant::store::{self, Memory};
use rusqlite::Connection;

const DIM: usize = 64;

fn memory(key: &str, title: &str, body: &str, tags: &[&str]) -> Memory {
    Memory {
        key: key.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        tags: tags.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn seed(conn: &Connection, embedder: &HashEmbedder, key: &str, body: &str, tags: &[&str]) {
    store::add(conn, embedder, &memory(key, key, body, tags)).expect("add");
}

/// A query with no constraints beyond the phrase, overridable by the caller.
fn query(text: Option<&str>) -> Query {
    Query {
        text: text.map(str::to_string),
        k: 10,
        max_distance: None,
        expand: 0,
    }
}

#[test]
fn semantic_ranks_the_closest_document_first() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "alpha-note",
        "alpha beta gamma delta",
        &[],
    );
    seed(
        &conn,
        &embedder,
        "other-note",
        "epsilon zeta eta theta",
        &[],
    );

    let hits = search::run(&conn, Some(&embedder), &query(Some("alpha beta gamma")))
        .expect("search")
        .hits;

    assert_eq!(hits[0].key, "alpha-note");
    assert!(
        hits[0].distance.is_some(),
        "a semantic hit must report a cosine distance"
    );
}

#[test]
fn tags_and_key_are_embedded() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    // The body says nothing about "postgres"; the tag and the key do.
    seed(
        &conn,
        &embedder,
        "db-backup",
        "snapshot nightly to cold storage",
        &["postgres"],
    );
    seed(
        &conn,
        &embedder,
        "web-note",
        "the frontend lives in apps/web",
        &[],
    );

    let hits = search::run(&conn, Some(&embedder), &query(Some("postgres")))
        .expect("search")
        .hits;
    assert_eq!(hits[0].key, "db-backup", "a tag token must be retrievable");
}

#[test]
fn semantic_needs_an_embedder() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let err = search::run(&conn, None, &query(Some("anything"))).unwrap_err();
    assert!(format!("{err}").contains("needs an embedder"));
}

#[test]
fn max_distance_filters_the_semantic_leg() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "alpha-note", "alpha beta gamma", &[]);
    seed(&conn, &embedder, "other-note", "epsilon zeta eta", &[]);

    // Cosine distance: the exact document is ~0, the unrelated one is ~1.
    let mut q = query(Some("alpha beta gamma"));
    q.max_distance = Some(0.5);
    let hits = search::run(&conn, Some(&embedder), &q)
        .expect("search")
        .hits;
    assert!(
        hits.iter().all(|h| h.distance.unwrap_or(1.0) <= 0.5),
        "got {hits:?}"
    );
    assert!(
        hits.iter().any(|h| h.key == "alpha-note"),
        "the exact match must survive the ceiling"
    );
    assert!(
        !hits.iter().any(|h| h.key == "other-note"),
        "the unrelated document must be filtered"
    );
}

#[test]
fn hits_carry_no_body() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "long-note", &"zebra ".repeat(200), &[]);

    let bundle = search::run(&conn, Some(&embedder), &query(Some("zebra"))).expect("search");
    assert_eq!(bundle.hits[0].title, "long-note");
    let json = serde_json::to_string(&bundle).unwrap();
    assert!(
        !json.contains("\"body\""),
        "a search hit must not carry a body: {json}"
    );
}

#[test]
fn expand_pulls_link_neighbours_and_backlinks() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "note-a", "start here [[note-b]]", &[]);
    seed(&conn, &embedder, "note-b", "middle [[note-c]]", &[]);
    seed(&conn, &embedder, "note-c", "the terminal note", &[]);

    // Outgoing: note-a -> note-b (hop 1) -> note-c (hop 2).
    let mut q = query(Some("start here"));
    q.k = 1; // only the top hit seeds the expansion
    q.expand = 2;
    let bundle = search::run(&conn, Some(&embedder), &q).expect("search");
    assert_eq!(bundle.hits[0].key, "note-a");
    let keys: Vec<(&str, &str, u8)> = bundle
        .neighbors
        .iter()
        .map(|n| (n.memory.key.as_str(), n.via.as_str(), n.hop))
        .collect();
    assert_eq!(keys, vec![("note-b", "note-a", 1), ("note-c", "note-b", 2)]);

    // Incoming: note-c is reached from its backlink note-b.
    let mut q = query(Some("terminal note"));
    q.k = 1;
    q.expand = 1;
    let bundle = search::run(&conn, Some(&embedder), &q).expect("search");
    assert_eq!(bundle.hits[0].key, "note-c");
    assert_eq!(bundle.neighbors.len(), 1);
    assert_eq!(bundle.neighbors[0].memory.key, "note-b");
}

#[test]
fn no_constraints_returns_everything_in_key_order() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "b", "two", &[]);
    seed(&conn, &embedder, "a", "one", &[]);

    let hits = search::run(&conn, None, &query(None)).expect("search").hits;
    assert_eq!(
        hits.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
}
