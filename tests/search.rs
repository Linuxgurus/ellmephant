//! Integration: the P5 read path against a real store file and a real `vec0` table.
//!
//! Runs offline — the deterministic [`HashEmbedder`] stands in for Qwen3. It is a
//! bag-of-words model, so "meaning" here reduces to shared tokens; that is enough to prove
//! the two legs run, fuse and hydrate without loading weights.

mod common;

use common::temp_store;
use llmphant::db;
use llmphant::embed::HashEmbedder;
use llmphant::search::{self, Mode, Query};
use llmphant::store::{self, Memory, MemoryType};
use rusqlite::Connection;

const DIM: usize = 64;

fn memory(key: &str, scope: &str, title: &str, body: &str) -> Memory {
    Memory {
        key: key.to_string(),
        scope: scope.to_string(),
        type_: MemoryType::Fact,
        title: title.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

fn seed(conn: &Connection, embedder: &HashEmbedder, key: &str, scope: &str, body: &str) {
    store::add(conn, embedder, &memory(key, scope, key, body)).expect("add");
}

fn query(text: &str, mode: Mode) -> Query {
    Query {
        text: text.to_string(),
        scope: None,
        k: 10,
        mode,
        max_distance: None,
        expand: 0,
    }
}

#[test]
fn vector_leg_ranks_the_closest_document_first() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "alpha-note",
        "infra",
        "alpha beta gamma delta",
    );
    seed(
        &conn,
        &embedder,
        "other-note",
        "infra",
        "epsilon zeta eta theta",
    );

    let hits = search::run(
        &conn,
        Some(&embedder),
        &query("alpha beta gamma", Mode::Vec),
    )
    .expect("vec search")
    .hits;

    assert_eq!(hits[0].key, "alpha-note");
    assert!(
        hits[0].distance.is_some(),
        "the KNN leg must report a cosine distance"
    );
}

#[test]
fn bm25_leg_finds_an_exact_identifier_without_a_model() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "portainer",
        "infra",
        "the gateway answers at portainer-notes-gw.internal",
    );
    seed(
        &conn,
        &embedder,
        "unrelated",
        "infra",
        "totally different content",
    );

    // `None` = no embedder; a lexical query must still work (PLAN §6 P5).
    let hits = search::run(&conn, None, &query("portainer-notes-gw", Mode::Fts))
        .expect("fts search")
        .hits;

    assert_eq!(hits[0].key, "portainer");
    assert!(hits[0].distance.is_none(), "fts leg has no distance");
}

#[test]
fn hybrid_run_requires_an_embedder_for_its_vector_leg() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let err = search::run(&conn, None, &query("anything", Mode::Hybrid)).unwrap_err();
    assert!(format!("{err}").contains("needs an embedder"));
}

#[test]
fn scope_filter_excludes_other_buckets() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "infra-note",
        "infra",
        "shared zebra token",
    );
    seed(
        &conn,
        &embedder,
        "media-note",
        "media",
        "shared zebra token",
    );

    let mut q = query("shared zebra token", Mode::Hybrid);
    q.scope = Some("media".to_string());
    let hits = search::run(&conn, Some(&embedder), &q)
        .expect("search")
        .hits;

    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| h.scope == "media"), "got {hits:?}");
}

#[test]
fn expand_pulls_link_neighbours_and_backlinks() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "note-a", "infra", "start here [[note-b]]");
    seed(&conn, &embedder, "note-b", "infra", "middle [[note-c]]");
    seed(&conn, &embedder, "note-c", "infra", "the terminal note");

    // Outgoing: note-a -> note-b (hop 1) -> note-c (hop 2).
    let mut q = query("start", Mode::Fts);
    q.expand = 2;
    let bundle = search::run(&conn, None, &q).expect("search");
    let keys: Vec<(&str, &str, u8)> = bundle
        .neighbors
        .iter()
        .map(|n| (n.memory.key.as_str(), n.via.as_str(), n.hop))
        .collect();
    assert_eq!(keys, vec![("note-b", "note-a", 1), ("note-c", "note-b", 2)]);

    // Incoming: note-c is reached from its backlink note-b.
    let mut q = query("terminal", Mode::Fts);
    q.expand = 1;
    let bundle = search::run(&conn, None, &q).expect("search");
    assert_eq!(bundle.neighbors.len(), 1);
    assert_eq!(bundle.neighbors[0].memory.key, "note-b");
}

#[test]
fn max_distance_filters_the_vector_leg() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "alpha-note", "infra", "alpha beta gamma");
    seed(&conn, &embedder, "other-note", "infra", "epsilon zeta eta");

    let mut q = query("alpha beta gamma", Mode::Vec);
    q.max_distance = Some(0.0); // only a perfect match survives
    let hits = search::run(&conn, Some(&embedder), &q)
        .expect("search")
        .hits;
    // The exact document is distance ~0; the unrelated one is filtered out.
    assert!(hits.iter().all(|h| h.distance.unwrap_or(1.0) <= 0.0));
}

#[test]
fn empty_query_returns_no_hits() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "alpha-note", "infra", "alpha beta");
    let hits = search::run(&conn, Some(&embedder), &query("   ", Mode::Fts))
        .expect("search")
        .hits;
    assert!(hits.is_empty());
}
