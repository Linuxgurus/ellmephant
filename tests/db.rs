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

fn drop_sent_under(conn: &Connection) {
    let has_mail: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'mail'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    if has_mail > 0 {
        let _ = conn.execute("ALTER TABLE mail DROP COLUMN sent_under", []);
    }
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

    for name in ["memory", "memory_link", "meta", "project", "config"] {
        assert!(object_exists(&conn, "table", name), "missing table {name}");
    }
    for column in ["set_audience", "read_audience"] {
        assert!(
            column_exists(&conn, "config", column),
            "config.{column} is missing"
        );
    }
    assert!(
        !object_exists(&conn, "table", "project_memory"),
        "project_memory should have been dropped by 0007"
    );
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
    for column in ["mem_preload", "start_date", "end_date"] {
        assert!(
            column_exists(&conn, "project", column),
            "project.{column} is missing"
        );
    }
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
fn the_recess_rows_are_retired_and_a_reopen_does_not_resurrect_them() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).unwrap();

    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM config WHERE key IN
                ('recess_daily_seconds','recess_sitting_seconds','recess_daily_grant_at')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        rows, 0,
        "the recess policy lives in brain.db; a second copy here is the copy nobody updates"
    );

    drop(conn);
    let conn = db::open(&paths).expect("reopen");
    let again: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM config WHERE key LIKE 'recess_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(again, 0, "a reopen must not put the retired rows back");
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

    let conn = db::open(&paths).unwrap();
    insert(&conn, "keep-me", "Kept", "survives the upgrade");
    conn.execute("DROP TABLE project", []).unwrap();
    conn.execute("ALTER TABLE memory_link DROP COLUMN to_type", [])
        .unwrap();
    conn.execute("DROP TABLE encode_queue", []).unwrap();
    conn.execute("DROP TABLE memory_version", []).unwrap();
    conn.execute("DROP TABLE claim", []).unwrap();
    conn.execute("DROP TABLE mail", []).unwrap();
    conn.execute("DROP TABLE mail_cursor", []).unwrap();
    conn.execute("DROP INDEX IF EXISTS idx_memory_asserted_by", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN asserted_by", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN origin", [])
        .unwrap();
    conn.execute(
        "ALTER TABLE memory ADD COLUMN coherent INTEGER CHECK (coherent IN (0, 1) OR coherent IS NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "ALTER TABLE memory ADD COLUMN doc_hash TEXT NOT NULL DEFAULT ''",
        [],
    )
    .unwrap();
    for t in ["memory_fts_ai", "memory_fts_ad", "memory_fts_au"] {
        conn.execute(&format!("DROP TRIGGER IF EXISTS {t}"), [])
            .unwrap();
    }
    conn.execute("DROP TABLE memory_fts", []).unwrap();
    conn.execute("DROP INDEX IF EXISTS memory_audience", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN audience", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN edited_by", [])
        .unwrap();
    conn.execute("DROP TABLE IF EXISTS config", []).unwrap();
    conn.execute("DROP TABLE pen", []).unwrap();
    conn.execute("DROP TABLE IF EXISTS active_sudo", [])
        .unwrap();
    conn.execute("DROP TABLE IF EXISTS sudoers", []).unwrap();
    conn.execute("DROP INDEX IF EXISTS memory_owner", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN owner", [])
        .unwrap();
    conn.execute("DROP TABLE pen_event", []).unwrap();
    conn.execute("DROP TABLE IF EXISTS private_read", [])
        .unwrap();
    conn.execute_batch(
        "DROP TABLE IF EXISTS channel_boundary;
         DROP TABLE IF EXISTS channel_event;
         DROP TABLE IF EXISTS channel_delivery;
         DROP TABLE IF EXISTS channel_message;
         DROP TABLE IF EXISTS channel_member;
         DROP TABLE IF EXISTS channel;
         DROP TABLE IF EXISTS mail_surrogate;
         DROP TABLE IF EXISTS claim_release_log;
         DROP TABLE IF EXISTS pen_credential;
         DROP TABLE IF EXISTS memory_fold;
         DROP TABLE IF EXISTS config;",
    )
    .unwrap();
    drop_sent_under(&conn);
    conn.pragma_update(None, "user_version", 4).unwrap();
    drop(conn);

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
    for t in ["memory_fts_ai", "memory_fts_ad", "memory_fts_au"] {
        conn.execute(&format!("DROP TRIGGER IF EXISTS {t}"), [])
            .unwrap();
    }
    conn.execute("DROP TABLE memory_fts", []).unwrap();
    conn.execute("DROP INDEX IF EXISTS memory_audience", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN audience", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN edited_by", [])
        .unwrap();
    conn.execute("DROP TABLE IF EXISTS config", []).unwrap();
    conn.execute("DROP TABLE pen", []).unwrap();
    conn.execute("DROP TABLE IF EXISTS active_sudo", [])
        .unwrap();
    conn.execute("DROP TABLE IF EXISTS sudoers", []).unwrap();
    conn.execute("DROP INDEX IF EXISTS memory_owner", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN owner", [])
        .unwrap();
    conn.execute("DROP TABLE pen_event", []).unwrap();
    conn.execute("DROP TABLE IF EXISTS private_read", [])
        .unwrap();
    conn.execute_batch(
        "DROP TABLE IF EXISTS channel_boundary;
         DROP TABLE IF EXISTS channel_event;
         DROP TABLE IF EXISTS channel_delivery;
         DROP TABLE IF EXISTS channel_message;
         DROP TABLE IF EXISTS channel_member;
         DROP TABLE IF EXISTS channel;
         DROP TABLE IF EXISTS mail_surrogate;
         DROP TABLE IF EXISTS claim_release_log;
         DROP TABLE IF EXISTS pen_credential;
         DROP TABLE IF EXISTS memory_fold;
         DROP TABLE IF EXISTS config;",
    )
    .unwrap();
    drop_sent_under(&conn);
    conn.pragma_update(None, "user_version", 5).unwrap();
    conn.execute("DROP TABLE encode_queue", []).unwrap();
    conn.execute("DROP TABLE memory_version", []).unwrap();
    conn.execute("DROP TABLE claim", []).unwrap();
    conn.execute("DROP TABLE mail", []).unwrap();
    conn.execute("DROP TABLE mail_cursor", []).unwrap();
    conn.execute("DROP INDEX IF EXISTS idx_memory_asserted_by", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN asserted_by", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN origin", [])
        .unwrap();
    conn.execute(
        "ALTER TABLE memory ADD COLUMN coherent INTEGER CHECK (coherent IN (0, 1) OR coherent IS NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "ALTER TABLE memory ADD COLUMN doc_hash TEXT NOT NULL DEFAULT ''",
        [],
    )
    .unwrap();
    drop(conn);

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

    assert!(
        !column_exists(&conn, "memory", "encode_hash"),
        "the truth table carries no derived stamp"
    );
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

#[test]
fn a_v20_store_gains_the_pen_registry_on_open() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).unwrap();

    insert(&conn, "said-by-stet", "Said by stet", "body");
    conn.execute(
        "UPDATE memory SET asserted_by = 'stet' WHERE key = 'said-by-stet'",
        [],
    )
    .unwrap();
    insert(&conn, "said-by-a-finger", "Said by a finger", "body");
    conn.execute(
        "UPDATE memory SET asserted_by = 'stet/finger-3' WHERE key = 'said-by-a-finger'",
        [],
    )
    .unwrap();

    conn.execute("DROP INDEX IF EXISTS memory_version_editor", [])
        .unwrap();
    conn.execute("ALTER TABLE memory_version DROP COLUMN edited_by", [])
        .unwrap();
    conn.execute("ALTER TABLE memory_version DROP COLUMN synthetic", [])
        .unwrap();
    conn.execute(
        "ALTER TABLE memory RENAME COLUMN edited_by TO last_written_by",
        [],
    )
    .unwrap();
    conn.execute("DROP TABLE IF EXISTS config", []).unwrap();
    conn.execute("DROP TABLE pen", []).unwrap();
    conn.execute("DROP TABLE IF EXISTS active_sudo", [])
        .unwrap();
    conn.execute("DROP TABLE IF EXISTS sudoers", []).unwrap();
    conn.execute("DROP INDEX IF EXISTS memory_owner", [])
        .unwrap();
    conn.execute("ALTER TABLE memory DROP COLUMN owner", [])
        .unwrap();
    conn.execute("DROP TABLE pen_event", []).unwrap();
    conn.execute("DROP TABLE IF EXISTS private_read", [])
        .unwrap();
    conn.execute_batch(
        "DROP TABLE IF EXISTS channel_boundary;
         DROP TABLE IF EXISTS channel_event;
         DROP TABLE IF EXISTS channel_delivery;
         DROP TABLE IF EXISTS channel_message;
         DROP TABLE IF EXISTS channel_member;
         DROP TABLE IF EXISTS channel;
         DROP TABLE IF EXISTS mail_surrogate;
         DROP TABLE IF EXISTS claim_release_log;
         DROP TABLE IF EXISTS pen_credential;
         DROP TABLE IF EXISTS memory_fold;
         DROP TABLE IF EXISTS config;",
    )
    .unwrap();
    drop_sent_under(&conn);
    conn.execute("PRAGMA user_version = 20", []).unwrap();
    drop(conn);

    let conn = db::open(&paths).expect("reopen");
    assert_eq!(db::migrate::version(&conn).unwrap(), db::migrate::latest());

    assert_eq!(
        count(&conn, "SELECT count(*) FROM pen WHERE name = 'stet'"),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM pen WHERE name = 'stet/finger-3'"
        ),
        1
    );

    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM pen WHERE name = 'stet/finger-3' AND kind = 'self' AND parent_id IS NULL"
        ),
        1,
        "a slash is an address, never a permission"
    );

    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM pen WHERE ceased_at IS NOT NULL"
        ),
        0
    );
}

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

    migrate::run_with(&conn, true).expect("the rollback override must work");
    assert_eq!(migrate::version(&conn).unwrap(), 99, "untouched");
}

#[test]
fn a_ghost_pen_called_anonymous_is_repaired_not_preserved() {
    let (_dir, paths) = temp_store();

    let conn = db::open(&paths).expect("open");
    conn.execute(
        "INSERT INTO memory (key, title, body, created, updated, asserted_by, owner, audience)
         VALUES ('ghosted', 't', 'b', datetime('now'), datetime('now'), 'anonymous',
                 (SELECT id FROM pen WHERE name = 'anonymous'), 'public')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO pen_event (key, action, pen_id) VALUES ('ghosted', 'adopt',
            (SELECT id FROM pen WHERE name = 'anonymous'))",
        [],
    )
    .unwrap();
    conn.execute("DROP TABLE IF EXISTS private_read", [])
        .unwrap();
    conn.execute_batch(
        "DROP TABLE IF EXISTS channel_boundary;
         DROP TABLE IF EXISTS channel_event;
         DROP TABLE IF EXISTS channel_delivery;
         DROP TABLE IF EXISTS channel_message;
         DROP TABLE IF EXISTS channel_member;
         DROP TABLE IF EXISTS channel;
         DROP TABLE IF EXISTS mail_surrogate;
         DROP TABLE IF EXISTS claim_release_log;
         DROP TABLE IF EXISTS pen_credential;
         DROP TABLE IF EXISTS memory_fold;
         DROP TABLE IF EXISTS config;",
    )
    .unwrap();
    drop_sent_under(&conn);
    conn.pragma_update(None, "user_version", 24).unwrap();
    drop(conn);

    let conn = db::open(&paths).expect("reopen");
    assert_eq!(db::migrate::version(&conn).unwrap(), db::migrate::latest());
    let ghost: i64 = conn
        .query_row(
            "SELECT count(*) FROM pen WHERE lower(name) = 'anonymous'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        ghost, 0,
        "`anonymous` is a value meaning nobody, never a pen"
    );

    let owner: Option<i64> = conn
        .query_row("SELECT owner FROM memory WHERE key = 'ghosted'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        owner, None,
        "un-owning restores the question rather than answering it"
    );

    let dangling: i64 = conn
        .query_row("SELECT count(*) FROM pen_event WHERE pen_id IS NOT NULL AND pen_id NOT IN (SELECT id FROM pen)", [], |r| r.get(0))
        .unwrap();
    assert_eq!(dangling, 0);
}
