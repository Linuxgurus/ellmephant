//! Integration tests: real SQLite file, real schema.
//!
//! Everything here runs offline — no model, no extension.

mod common;

use common::{count, temp_store};
use ellmephant::db;
use rusqlite::{Connection, params};

fn object_exists(conn: &Connection, kind: &str, name: &str) -> bool {
    count(
        conn,
        &format!("SELECT count(*) FROM sqlite_master WHERE type='{kind}' AND name='{name}'"),
    ) > 0
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("table_info");
    let names = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .expect("columns");
    names
        .filter_map(std::result::Result::ok)
        .any(|c| c == column)
}

fn insert(conn: &Connection, key: &str, title: &str, body: &str) {
    conn.execute(
        "INSERT INTO memory (key, title, body, doc_hash) VALUES (?1, ?2, ?3, 'h')",
        params![key, title, body],
    )
    .expect("insert memory");
}

#[test]
fn fresh_store_is_at_latest_schema_with_every_object() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    assert_eq!(db::migrate::version(&conn).unwrap(), db::migrate::latest());
    assert!(db::migrate::latest() >= 2, "telemetry migration exists");
    assert!(db::migrate::latest() >= 5, "projects migration exists");
    assert!(db::migrate::latest() >= 6, "link-type migration exists");
    assert!(db::migrate::latest() >= 7, "one-home removal exists");
    assert!(db::migrate::latest() >= 8, "encode queue exists");

    for name in ["memory", "memory_link", "meta", "project"] {
        assert!(object_exists(&conn, "table", name), "missing table {name}");
    }
    // 0007 retired the one-home association: membership is a tag now, so the table must be GONE —
    // and must never creep back.
    assert!(
        !object_exists(&conn, "table", "project_memory"),
        "project_memory should have been dropped by 0007"
    );
    // v2 keeps one embedding per row: guard against a BM25/tag table being reintroduced.
    for name in ["memory_fts", "tag", "memory_tag"] {
        assert!(
            !object_exists(&conn, "table", name),
            "{name} should not exist"
        );
    }
    // The preload list is a COLUMN on `project`, not a tie table (0005's header says why):
    // guard against a `project_preload` table creeping back.
    for column in ["mem_preload", "start_date", "end_date"] {
        assert!(
            column_exists(&conn, "project", column),
            "project.{column} is missing"
        );
    }
    // v6: an edge says which table its bare key names.
    assert!(
        column_exists(&conn, "memory_link", "to_type"),
        "memory_link.to_type is missing"
    );
    assert!(
        !object_exists(&conn, "table", "project_preload"),
        "preload is a column, not a table"
    );
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
    assert_eq!(db::migrate::version(&conn).unwrap(), db::migrate::latest());
    assert_eq!(count(&conn, "SELECT count(*) FROM memory"), 1);
}

#[test]
fn a_v4_store_gains_the_project_tables_on_open() {
    let (_dir, paths) = temp_store();

    // A store from before projects existed: the tables gone, the link type gone, and the version
    // rolled back.
    let conn = db::open(&paths).unwrap();
    insert(&conn, "keep-me", "Kept", "survives the upgrade");
    conn.execute("DROP TABLE project", []).unwrap();
    conn.execute("ALTER TABLE memory_link DROP COLUMN to_type", [])
        .unwrap();
    // A v4 store predates the encode queue as well.
    conn.execute("DROP TABLE encode_queue", []).unwrap();
    conn.pragma_update(None, "user_version", 4).unwrap();
    drop(conn);

    // Opening it again runs only the newer steps, and the rows are untouched.
    let conn = db::open(&paths).expect("reopen");
    assert_eq!(db::migrate::version(&conn).unwrap(), db::migrate::latest());
    assert!(object_exists(&conn, "table", "project"), "missing project");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM memory"),
        1,
        "an upgrade must not touch stored rows"
    );
}

#[test]
fn a_v5_store_gets_bare_keys_and_typed_edges() {
    let (_dir, paths) = temp_store();

    // The state an earlier v5 build left behind: prefixed project keys, a one-home membership row,
    // and edges that named them by prefix — with no `to_type` column to say so.
    let conn = db::open(&paths).unwrap();
    conn.execute("ALTER TABLE memory_link DROP COLUMN to_type", [])
        .unwrap();
    insert(&conn, "source", "Source", "points at [[prj-chronos]]");
    let from_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO project (key, title) VALUES ('prj-chronos', 'Chronos')",
        [],
    )
    .unwrap();
    // A v5 store still HOLDS the one-home table (0007 has not run yet), so the fixture rebuilds it —
    // otherwise 0006's backfill below would have nothing to rewrite.
    conn.execute(
        "CREATE TABLE project_memory (memory_key TEXT PRIMARY KEY, project_key TEXT NOT NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO project_memory (memory_key, project_key) VALUES ('source', 'prj-chronos')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_link (from_id, to_key, rel) VALUES (?1, 'prj-chronos', 'see')",
        params![from_id],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 5).unwrap();
    // A v5 store predates the encode queue too.
    conn.execute("DROP TABLE encode_queue", []).unwrap();
    drop(conn);

    // 0006 interprets the prefix exactly once — while it still means something — and 0007 then
    // retires the one-home table entirely.
    let conn = db::open(&paths).expect("reopen");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM project WHERE key = 'chronos'"),
        1,
        "the project key lost its prefix"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM memory_link WHERE to_key = 'chronos' AND to_type = 'project'"
        ),
        1,
        "and the edge is now typed, with its key stripped"
    );
    assert!(
        !object_exists(&conn, "table", "project_memory"),
        "0007 dropped the one-home table on the way through"
    );
    assert!(
        ellmephant::store::links::dangling(&conn)
            .unwrap()
            .is_empty(),
        "the target exists, so nothing dangles"
    );
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
