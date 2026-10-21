//! Integration tests: real SQLite file, real schema.
//!
//! Everything here runs offline — no model, no extension.

mod common;

use common::{count, temp_store};
use ellmephant::db;
use ellmephant::db::migrate;
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
        "INSERT INTO memory (key, title, body) VALUES (?1, ?2, ?3)",
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
    // 0007 dropped the one-home association: membership is a tag now, so the table must be GONE —
    // and must never creep back.
    assert!(
        !object_exists(&conn, "table", "project_memory"),
        "project_memory should have been dropped by 0007"
    );
    // ★ THE v2 DECISION IS REVERSED FOR THE WORD INDEX (soma/ellmephant#62, 2026-10-22): a single
    // semantic phrase could not find a name-shaped query — the right memory ranked FIFTH at 0.5945
    // behind an unrelated 0.5795 — so `memory_fts` is now INTENDED, and its absence would be the bug.
    // The tag tables stay absent: a tag is a column, and a join table for it was never right.
    assert!(
        object_exists(&conn, "table", "memory_fts"),
        "the word index is now part of the schema (0017)"
    );
    for name in ["tag", "memory_tag"] {
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
    // ...and predates memory versions, which 0011 recreates: a fake old store has to UNDO what the
    // newer migrations add, or the migration collides with a table that never went away.
    conn.execute("DROP TABLE memory_version", []).unwrap();
    // ...and predates claims (0012), for the same reason.
    conn.execute("DROP TABLE claim", []).unwrap();
    // ...and predates MAIL (0013), for the same reason: a fake old store must UNDO what the newer
    // migrations add, or the migration collides with a table that never went away.
    conn.execute("DROP TABLE mail", []).unwrap();
    // ...and predates the MAIL CURSOR (0017), which is a table of its own.
    conn.execute("DROP TABLE mail_cursor", []).unwrap();
    // ...and predates PROVENANCE (0014): undo its columns, index first — SQLite refuses to drop an
    // indexed column.
    conn.execute("DROP INDEX IF EXISTS idx_memory_asserted_by", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN asserted_by", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN origin", [])
        .unwrap();
    // ...and it still HOLDS the split-review column: 0004 added it, and 0009 is what drops it.
    conn.execute(
        "ALTER TABLE memory ADD COLUMN coherent INTEGER CHECK (coherent IN (0, 1) OR coherent IS NULL)",
        [],
    )
    .unwrap();
    // And it still spells the column `doc_hash`: 0010 is the rename that fixes that, and 0016 is the
    // migration that removes it entirely — so the OLD store is built by ADDING the old spelling, not
    // by renaming a column this schema no longer has.
    conn.execute(
        "ALTER TABLE memory ADD COLUMN doc_hash TEXT NOT NULL DEFAULT ''",
        [],
    )
    .unwrap();
    // ...and predates the WORD INDEX (0017): drop its triggers BEFORE the table, or a trigger body
    // survives naming a table that no longer exists and every later write fails.
    for t in ["memory_fts_ai", "memory_fts_ad", "memory_fts_au"] {
        conn.execute(&format!("DROP TRIGGER IF EXISTS {t}"), [])
            .unwrap();
    }
    conn.execute("DROP TABLE memory_fts", []).unwrap();
    // ...and predates the READ AUDIENCE (0018): drop the column the migration would add.
    conn.execute("DROP INDEX IF EXISTS memory_audience", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN audience", [])
        .unwrap();
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
    // ...and predates the WORD INDEX (0017): drop its triggers BEFORE the table, or a trigger body
    // survives naming a table that no longer exists and every later write fails.
    for t in ["memory_fts_ai", "memory_fts_ad", "memory_fts_au"] {
        conn.execute(&format!("DROP TRIGGER IF EXISTS {t}"), [])
            .unwrap();
    }
    conn.execute("DROP TABLE memory_fts", []).unwrap();
    conn.execute("DROP INDEX IF EXISTS memory_audience", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN audience", [])
        .unwrap();
    conn.pragma_update(None, "user_version", 5).unwrap();
    // A v5 store predates the encode queue too.
    conn.execute("DROP TABLE encode_queue", []).unwrap();
    // ...and predates memory versions (0011), for the same reason as above.
    conn.execute("DROP TABLE memory_version", []).unwrap();
    // ...and predates claims (0012), for the same reason.
    conn.execute("DROP TABLE claim", []).unwrap();
    // ...and predates MAIL (0013), for the same reason: a fake old store must UNDO what the newer
    // migrations add, or the migration collides with a table that never went away.
    conn.execute("DROP TABLE mail", []).unwrap();
    // ...and predates the MAIL CURSOR (0017), which is a table of its own.
    conn.execute("DROP TABLE mail_cursor", []).unwrap();
    // ...and predates PROVENANCE (0014), for the same reason.
    conn.execute("DROP INDEX IF EXISTS idx_memory_asserted_by", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN asserted_by", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN origin", [])
        .unwrap();
    // ...and still HOLDS the split-review column: 0004 added it, and 0009 is what drops it.
    conn.execute(
        "ALTER TABLE memory ADD COLUMN coherent INTEGER CHECK (coherent IN (0, 1) OR coherent IS NULL)",
        [],
    )
    .unwrap();
    // …and still carries the old `doc_hash` spelling, which 0010 renames and 0016 removes.
    conn.execute(
        "ALTER TABLE memory ADD COLUMN doc_hash TEXT NOT NULL DEFAULT ''",
        [],
    )
    .unwrap();
    drop(conn);

    // 0006 interprets the prefix exactly once — while it still means something — and 0007 then
    // drops the one-home table entirely.
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
fn memory_keeps_tags_and_no_scope_or_type() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).unwrap();
    conn.execute(
        "INSERT INTO memory (key, title, body, tags) VALUES ('a', 'T', 'B', 'k8s,infra')",
        [],
    )
    .unwrap();

    let tags: String = conn
        .query_row("SELECT tags FROM memory WHERE key = 'a'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tags, "k8s,infra");

    // ★ NO ENCODING STATE ON THE ROW (0016): a document records what it IS, never whether its vector
    // exists. The queue and the index answer that, and a stamp here was the bug.
    assert!(
        !column_exists(&conn, "memory", "encode_hash"),
        "the truth table carries no derived stamp"
    );
    // `tags` defaults to the empty string.
    conn.execute(
        "INSERT INTO memory (key, title, body) VALUES ('c', 'T', 'B')",
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
        "INSERT INTO memory (key, title, body) VALUES ('dup', 'Two', 'body')",
        [],
    );
    assert!(
        err.is_err(),
        "duplicate keys must be rejected by the UNIQUE constraint"
    );
}

// The skipped-migration (hole in the list) case is unit-tested in `src/db/migrate.rs`, because a
// store at a version *below* the binary's highest and *unknown to it* cannot be built against a
// contiguous list: with 1..15 present, every lower version is known. The synthetic-list test is the
// honest way to exercise it.

/// The opposite state: the STORE IS NEWER than this binary, which is what a rollback looks like.
/// Refusing here would make a bad deploy unrecoverable, so it is allowed — but only deliberately.
#[test]
fn a_store_ahead_of_this_binary_can_only_be_entered_deliberately() {
    let (_dir, paths) = temp_store();
    {
        let conn = db::open(&paths).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }

    let conn = Connection::open(paths.db()).unwrap();
    let err = migrate::run_with(&conn, false).expect_err("a rollback must be refused by default");
    let text = format!("{err}");
    assert!(
        text.contains("STORE IS NEWER") && text.contains("ROLLBACK") || text.contains("rollback"),
        "the refusal must name the direction, not just the number: {text}"
    );
    assert!(
        text.contains(migrate::ALLOW_STORE_AHEAD),
        "and it must name the escape: {text}"
    );

    // The deliberate override proceeds, and applies nothing (the store is already beyond this list).
    migrate::run_with(&conn, true).expect("the rollback override must work");
    assert_eq!(migrate::version(&conn).unwrap(), 99, "untouched");
}
