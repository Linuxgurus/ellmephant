//! Integration tests: real SQLite file, real schema.
//!
//! Everything here runs offline — no model, no extension.

mod common;

use common::{count, temp_store};
use llmphant::db;
use rusqlite::{Connection, params};

fn object_exists(conn: &Connection, kind: &str, name: &str) -> bool {
    count(
        conn,
        &format!("SELECT count(*) FROM sqlite_master WHERE type='{kind}' AND name='{name}'"),
    ) > 0
}

fn insert(conn: &Connection, key: &str, title: &str, body: &str) {
    conn.execute(
        "INSERT INTO memory (key, title, body, doc_hash) VALUES (?1, ?2, ?3, 'h')",
        params![key, title, body],
    )
    .expect("insert memory");
}

#[test]
fn fresh_store_is_at_schema_v1_with_every_object() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    assert_eq!(db::migrate::version(&conn).unwrap(), 1);
    assert_eq!(db::migrate::latest(), 1);

    for name in ["memory", "memory_link", "meta"] {
        assert!(object_exists(&conn, "table", name), "missing table {name}");
    }
    // v2 keeps one embedding per row: guard against a BM25/tag table being reintroduced.
    for name in ["memory_fts", "tag", "memory_tag"] {
        assert!(
            !object_exists(&conn, "table", name),
            "{name} should not exist"
        );
    }
    assert!(
        object_exists(&conn, "index", "idx_link_to"),
        "missing index"
    );
    assert!(paths.db().exists(), "memory.db was not created");
}

#[test]
fn pragmas_are_applied() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).unwrap();

    let fk: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk, 1, "foreign_keys must be ON");

    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}

#[test]
fn migrations_are_idempotent_across_reopen() {
    let (_dir, paths) = temp_store();

    let conn = db::open(&paths).unwrap();
    insert(&conn, "keep-me", "Kept", "survives reopen");
    drop(conn);

    let conn = db::open(&paths).expect("second open");
    assert_eq!(db::migrate::version(&conn).unwrap(), 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM memory"), 1);
}

#[test]
fn memory_keeps_a_doc_hash_tags_and_no_scope_or_type() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).unwrap();
    conn.execute(
        "INSERT INTO memory (key, title, body, tags, doc_hash)
         VALUES ('a', 'T', 'B', 'k8s,infra', 'dh')",
        [],
    )
    .unwrap();

    let (tags, doc_hash): (String, String) = conn
        .query_row(
            "SELECT tags, doc_hash FROM memory WHERE key = 'a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((tags.as_str(), doc_hash.as_str()), ("k8s,infra", "dh"));

    // `doc_hash` is NOT NULL; `tags` defaults to the empty string.
    assert!(
        conn.execute(
            "INSERT INTO memory (key, title, body) VALUES ('b', 'T', 'B')",
            [],
        )
        .is_err()
    );
    conn.execute(
        "INSERT INTO memory (key, title, body, doc_hash) VALUES ('c', 'T', 'B', 'dh')",
        [],
    )
    .unwrap();
    let tags: String = conn
        .query_row("SELECT tags FROM memory WHERE key = 'c'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tags, "");
}

#[test]
fn deleting_a_memory_cascades_to_links() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).unwrap();

    insert(&conn, "a", "Anchor", "see [[ghost]]");
    conn.execute(
        "INSERT INTO memory_link (from_id, to_key, rel)
         SELECT id, 'ghost', 'see' FROM memory WHERE key = 'a'",
        [],
    )
    .unwrap();

    conn.execute("DELETE FROM memory WHERE key = 'a'", [])
        .unwrap();

    assert_eq!(count(&conn, "SELECT count(*) FROM memory_link"), 0);
}

#[test]
fn key_is_unique() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).unwrap();

    insert(&conn, "dup", "One", "body");
    let err = conn.execute(
        "INSERT INTO memory (key, title, body, doc_hash) VALUES ('dup', 'Two', 'body', 'h')",
        [],
    );
    assert!(
        err.is_err(),
        "duplicate keys must be rejected by the UNIQUE constraint"
    );
}
