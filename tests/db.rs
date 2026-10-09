//! Integration tests: real SQLite file, real schema, real FTS triggers.
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
        "INSERT INTO memory (key, scope, type, title, body, hash)
         VALUES (?1, 'infra', 'fact', ?2, ?3, 'deadbeef')",
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

    for name in [
        "memory",
        "tag",
        "memory_tag",
        "memory_fts",
        "memory_link",
        "meta",
    ] {
        assert!(object_exists(&conn, "table", name), "missing table {name}");
    }
    for name in ["memory_ai", "memory_ad", "memory_au"] {
        assert!(
            object_exists(&conn, "trigger", name),
            "missing trigger {name}"
        );
    }
    for name in [
        "idx_memory_scope",
        "idx_memory_type",
        "idx_mtag_tag",
        "idx_link_to",
    ] {
        assert!(object_exists(&conn, "index", name), "missing index {name}");
    }
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
fn fts_stays_in_sync_on_insert_update_delete() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).unwrap();

    insert(&conn, "a", "Slow loris", "the quick brown fox");
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM memory_fts WHERE memory_fts MATCH 'brown'"
        ),
        1
    );

    conn.execute(
        "UPDATE memory SET body = 'nothing left here' WHERE key = 'a'",
        [],
    )
    .unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM memory_fts WHERE memory_fts MATCH 'brown'"
        ),
        0,
        "stale token survived an update"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM memory_fts WHERE memory_fts MATCH 'nothing'"
        ),
        1
    );

    conn.execute("DELETE FROM memory WHERE key = 'a'", [])
        .unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM memory_fts WHERE memory_fts MATCH 'nothing'"
        ),
        0
    );
}

#[test]
fn deleting_a_memory_cascades_to_tags_and_links() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).unwrap();

    insert(&conn, "a", "Anchor", "see [[ghost]]");
    conn.execute("INSERT INTO tag (name) VALUES ('k8s')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO memory_tag (memory_id, tag_id)
         SELECT m.id, t.id FROM memory m, tag t WHERE m.key = 'a' AND t.name = 'k8s'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_link (from_id, to_key, rel)
         SELECT id, 'ghost', 'see' FROM memory WHERE key = 'a'",
        [],
    )
    .unwrap();

    conn.execute("DELETE FROM memory WHERE key = 'a'", [])
        .unwrap();

    assert_eq!(count(&conn, "SELECT count(*) FROM memory_tag"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_link"), 0);
    // The tag itself is shared vocabulary and must survive the link going away.
    assert_eq!(count(&conn, "SELECT count(*) FROM tag"), 1);
}

#[test]
fn key_is_unique() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).unwrap();

    insert(&conn, "dup", "One", "body");
    let err = conn.execute(
        "INSERT INTO memory (key, scope, title, body, hash)
         VALUES ('dup', 'infra', 'Two', 'body', 'h')",
        [],
    );
    assert!(
        err.is_err(),
        "duplicate keys must be rejected by the UNIQUE constraint"
    );
}
