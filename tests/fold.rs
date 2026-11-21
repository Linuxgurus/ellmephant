mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::store::{self, Memory, links};
use rusqlite::Connection;

const DIM: usize = 4;

fn memory(key: &str, body: &str) -> Memory {
    Memory {
        key: key.to_string(),
        title: format!("{key} title"),
        body: body.to_string(),
        tags: vec!["keep-this-tag".into()],
        ..Default::default()
    }
}

fn pen(conn: &Connection, name: &str) {
    conn.execute(
        "INSERT OR IGNORE INTO pen (name, kind) VALUES (?1, 'self')",
        rusqlite::params![name],
    )
    .expect("pen");
}

fn own(conn: &Connection, key: &str, pen_name: &str) {
    conn.execute(
        "UPDATE memory SET owner = (SELECT id FROM pen WHERE name = ?2) WHERE key = ?1",
        rusqlite::params![key, pen_name],
    )
    .expect("owner");
}

fn seed(conn: &Connection, embedder: &HashEmbedder, key: &str, body: &str) {
    common::add_now(conn, embedder, &memory(key, body)).expect("add");
}

#[test]
fn a_peer_folds_without_write_authority_and_the_source_survives() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    pen(&conn, "pen-a");
    pen(&conn, "pen-b");

    seed(&conn, &embedder, "keeper", "line one\nline two");
    seed(&conn, &embedder, "duplicate", "line two\nline three");
    own(&conn, "keeper", "pen-a");
    own(&conn, "duplicate", "pen-b");

    let refused = links::merge_into(&conn, "duplicate", "keeper", Some("pen-b"))
        .expect_err("a peer has no write authority on the survivor");
    assert!(
        format!("{refused}").contains("owner") || format!("{refused}").contains("may not"),
        "{refused}"
    );

    let out = links::fold_into(&conn, "duplicate", "keeper", Some("pen-b")).expect("fold");
    assert!(out.changed);
    assert_eq!(out.into, "keeper");
    assert_eq!(out.from, "duplicate");
    assert_eq!(out.integrated, vec!["line three".to_string()]);

    let keeper = store::get(&conn, "keeper").unwrap().unwrap();
    assert_eq!(keeper.body, "line one\nline two\nline three");
    assert_eq!(keeper.title, "keeper title");
    assert_eq!(keeper.tags, vec!["keep-this-tag".to_string()]);
    assert_eq!(keeper.audience, "public");
    assert_eq!(keeper.edited_by.as_deref(), Some("pen-b"));

    let source = store::get(&conn, "duplicate")
        .unwrap()
        .expect("source survives");
    assert_eq!(source.body, "line two\nline three");
    assert!(source.owner.is_some(), "the source keeps its owner");

    let (absorbed, actor): (String, Option<String>) = conn
        .query_row(
            "SELECT absorbed_key, actor FROM memory_fold WHERE keeper_id = (SELECT id FROM memory WHERE key = 'keeper')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(absorbed, "duplicate");
    assert_eq!(actor.as_deref(), Some("pen-b"));
}

#[test]
fn the_fold_works_in_both_directions() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    pen(&conn, "pen-a");
    pen(&conn, "pen-b");

    seed(&conn, &embedder, "a", "alpha\nshared");
    seed(&conn, &embedder, "b", "beta\nshared");
    own(&conn, "a", "pen-a");
    own(&conn, "b", "pen-b");

    links::fold_into(&conn, "b", "a", Some("pen-b")).expect("b -> a");
    links::fold_into(&conn, "a", "b", Some("pen-a")).expect("a -> b");

    assert!(
        store::get(&conn, "a")
            .unwrap()
            .unwrap()
            .body
            .contains("beta")
    );
    assert!(
        store::get(&conn, "b")
            .unwrap()
            .unwrap()
            .body
            .contains("alpha")
    );
    let b = store::get(&conn, "b").unwrap().unwrap().body;
    assert_eq!(b.matches("shared").count(), 1, "{b}");
}

#[test]
fn a_fold_that_has_nothing_to_add_writes_nothing() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    pen(&conn, "pen-a");
    pen(&conn, "pen-b");

    seed(&conn, &embedder, "keeper", "the same line");
    seed(&conn, &embedder, "duplicate", "the same line");
    own(&conn, "keeper", "pen-a");
    own(&conn, "duplicate", "pen-b");
    let before = store::get(&conn, "keeper").unwrap().unwrap();
    let out = links::fold_into(&conn, "duplicate", "keeper", Some("pen-b")).expect("fold");
    assert!(!out.changed);
    assert!(out.integrated.is_empty());
    let after = store::get(&conn, "keeper").unwrap().unwrap();
    assert_eq!(before.body, after.body);
    assert_eq!(
        before.edited_by, after.edited_by,
        "no write means no writer"
    );
    let folds: i64 = conn
        .query_row("SELECT count(*) FROM memory_fold", [], |r| r.get(0))
        .unwrap();
    assert_eq!(folds, 0, "a no-op is not a fold");
}

#[test]
fn you_may_not_fold_a_row_you_cannot_read() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    pen(&conn, "pen-a");
    pen(&conn, "pen-b");

    seed(&conn, &embedder, "keeper", "private words");
    seed(&conn, &embedder, "duplicate", "other words");
    own(&conn, "keeper", "pen-a");
    own(&conn, "duplicate", "pen-b");
    conn.execute(
        "UPDATE memory SET audience = 'pen:pen-a' WHERE key = 'keeper'",
        [],
    )
    .unwrap();

    let err = links::fold_into(&conn, "duplicate", "keeper", Some("pen-b"))
        .expect_err("cannot append to a row you cannot read");
    assert!(format!("{err}").contains("audience"), "{err}");
    assert_eq!(
        store::get(&conn, "keeper").unwrap().unwrap().body,
        "private words"
    );
}

#[test]
fn an_unproven_caller_cannot_fold_even_a_public_row() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    pen(&conn, "pen-a");
    seed(&conn, &embedder, "keeper", "public words");
    seed(&conn, &embedder, "duplicate", "other words");
    own(&conn, "keeper", "pen-a");

    let err = links::fold_into(&conn, "duplicate", "keeper", None)
        .expect_err("an append must name the pen doing it");
    assert!(format!("{err}").contains("must name"), "{err}");

    let keeper = store::get(&conn, "keeper").unwrap().unwrap();
    assert_eq!(keeper.body, "public words");
    assert_eq!(keeper.edited_by.as_deref(), Some("anonymous"));
    let folds: i64 = conn
        .query_row("SELECT count(*) FROM memory_fold", [], |r| r.get(0))
        .unwrap();
    assert_eq!(folds, 0, "a refused fold records nothing");
}

#[test]
fn a_missing_row_is_a_refusal_not_a_silent_no_op() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "keeper", "here");

    assert!(links::fold_into(&conn, "ghost", "keeper", Some("pen-a")).is_err());
    assert!(links::fold_into(&conn, "keeper", "ghost", Some("pen-a")).is_err());
    assert!(links::fold_into(&conn, "keeper", "keeper", Some("pen-a")).is_err());
}
