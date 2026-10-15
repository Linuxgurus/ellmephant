//! Integration: link-graph maintenance — `rename` and the dangling audit (P9).
//!
//! Runs offline against a real store file; the deterministic [`HashEmbedder`] stands in for
//! Qwen3. The sharpest assertions are that a rename rewrites the `[[…]]` tokens in *bodies*
//! and repoints the `memory_link` *edges* in the same transaction.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::store::{self, Memory, links};
use rusqlite::Connection;

const DIM: usize = 8;

fn memory(key: &str, body: &str) -> Memory {
    Memory {
        key: key.to_string(),
        title: key.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

fn seed(conn: &Connection, embedder: &HashEmbedder, key: &str, body: &str) {
    store::add(conn, embedder, &memory(key, body)).expect("add");
}

#[test]
fn rename_rewrites_bodies_and_repoints_edges() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    seed(&conn, &embedder, "target", "the target itself");
    seed(&conn, &embedder, "a", "see [[target]] first");
    seed(&conn, &embedder, "b", "also [[target]] here");

    let outcome = links::rename(&conn, &embedder, "target", "renamed").expect("rename");
    assert_eq!(outcome.key, "renamed");
    assert_eq!(outcome.bodies, 2, "both bodies carried the token");
    assert_eq!(outcome.edges, 2, "both edges repointed");

    // The row moved.
    assert!(store::get(&conn, "target").unwrap().is_none());
    assert!(store::get(&conn, "renamed").unwrap().is_some());

    // Every body now points at the new key (no stale `[[target]]`).
    for key in ["a", "b"] {
        let body = store::get(&conn, key).unwrap().unwrap().body;
        assert!(body.contains("[[renamed]]"), "{key}: {body}");
        assert!(!body.contains("[[target]]"), "{key}: {body}");
    }
    let body = store::get(&conn, "renamed").unwrap().unwrap().body;
    assert_eq!(body, "the target itself", "the row's own body is untouched");

    // The edges repointed in lockstep; nothing is dangling.
    let keys: Vec<String> = conn
        .prepare("SELECT to_key FROM memory_link ORDER BY to_key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(keys, vec!["renamed", "renamed"]);
    assert!(links::dangling(&conn).unwrap().is_empty());
}

#[test]
fn rename_refuses_missing_and_colliding_keys() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "one", "first");
    seed(&conn, &embedder, "two", "second");

    assert!(
        links::rename(&conn, &embedder, "ghost", "new").is_err(),
        "missing old"
    );
    let err = links::rename(&conn, &embedder, "one", "two").unwrap_err();
    assert!(format!("{err}").contains("already exists"), "{err}");
    assert!(store::get(&conn, "one").unwrap().is_some(), "unchanged");
}

#[test]
fn rename_to_the_same_key_is_a_noop() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "one", "body");

    let outcome = links::rename(&conn, &embedder, "one", "one").expect("rename");
    assert_eq!((outcome.bodies, outcome.edges), (0, 0));
    assert!(store::get(&conn, "one").unwrap().is_some());
}

#[test]
fn rename_re_embeds_the_row_it_moves() {
    // `key` IS in the embed projection, so a rename moves the row's *document*. Its stored
    // `doc_hash` (and vector) must describe the NEW key; otherwise the vector drifts until
    // something else happens to write the row.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    // `memory()` uses the key as the title too, so the title stays put across the rename.
    seed(&conn, &embedder, "old-name", "the body");

    links::rename(&conn, &embedder, "old-name", "new-name").expect("rename");

    let stored: String = conn
        .query_row(
            "SELECT doc_hash FROM memory WHERE key = 'new-name'",
            [],
            |r| r.get(0),
        )
        .expect("row moved");
    let fresh = store::row_hash(
        &embedder,
        &ellmephant::embed::projection::embed_document("new-name", &[], "old-name", "the body"),
    );
    assert_eq!(
        stored, fresh,
        "the row was re-embedded under its new key, not left describing the old one"
    );

    // The pre-fix hash (documented under the OLD key) must NOT match — that drift is the bug.
    let stale = store::row_hash(
        &embedder,
        &ellmephant::embed::projection::embed_document("old-name", &[], "old-name", "the body"),
    );
    assert_ne!(stored, stale, "the stale, old-key hash was replaced");
}

#[test]
fn dangling_audit_groups_unresolved_targets() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    seed(&conn, &embedder, "present", "resolves");
    seed(&conn, &embedder, "a", "links [[ghost]] and [[present]]");
    seed(
        &conn,
        &embedder,
        "b",
        "also links [[ghost]] and [[phantom]]",
    );

    let dangling = links::dangling(&conn).expect("dangling");
    assert_eq!(dangling.len(), 2, "two unresolved targets: {dangling:?}");
    assert_eq!(dangling[0].to_key, "ghost");
    assert_eq!(
        dangling[0].from_keys,
        vec!["a".to_string(), "b".to_string()]
    );
    assert_eq!(dangling[1].to_key, "phantom");
    assert_eq!(dangling[1].from_keys, vec!["b".to_string()]);
}

#[test]
fn renaming_clears_a_dangling_edge() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    // `a` references a key that does not exist yet — a forward reference.
    seed(&conn, &embedder, "a", "forward [[later]]");
    assert_eq!(links::dangling(&conn).unwrap().len(), 1);

    // Renaming an unrelated memory into `later` resolves it (no edge repoint needed).
    seed(&conn, &embedder, "stub", "placeholder");
    links::rename(&conn, &embedder, "stub", "later").expect("rename");
    assert!(links::dangling(&conn).unwrap().is_empty());
}

#[test]
fn a_link_to_a_project_resolves_and_is_not_rot() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    // A memory may link to a project — the `prj:` marker is what makes a key cross between the
    // two tables without either graph knowing about the other.
    ellmephant::store::project::upsert(
        &conn,
        &ellmephant::store::project::Project {
            key: "chronos".into(),
            title: "Chronos".into(),
            ..Default::default()
        },
        None,
    )
    .expect("project");
    seed(
        &conn,
        &embedder,
        "a",
        "works in [[prj:chronos]] and [[ghost]]",
    );

    let dangling = links::dangling(&conn).expect("dangling");
    assert_eq!(dangling.len(), 1, "only the ghost is rot: {dangling:?}");
    assert_eq!(dangling[0].to_key, "ghost");
}
