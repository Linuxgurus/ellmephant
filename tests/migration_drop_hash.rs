use ellmephant::db;
use rusqlite::Connection;

mod common;

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("pragma");
    stmt.query_map([], |r| r.get::<_, String>(1))
        .expect("rows")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect")
}

#[test]
fn a_migrated_store_names_encode_hash_nowhere() {
    let (_dir, paths) = common::temp_store();
    let conn = db::open(&paths).expect("open");

    assert!(
        !columns(&conn, "memory").iter().any(|c| c == "encode_hash"),
        "the column is gone"
    );

    let mut stmt = conn
        .prepare("SELECT type, name, COALESCE(sql, '') FROM sqlite_master WHERE type IN ('trigger','view')")
        .expect("master");
    let named = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .expect("rows")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect")
        .into_iter()
        .filter(|(_, _, sql)| sql.contains("encode_hash"))
        .map(|(kind, name, _)| format!("{kind} {name}"))
        .collect::<Vec<_>>();

    assert!(
        named.is_empty(),
        "a trigger or view still names encode_hash: {named:?} — DROP COLUMN will not warn about a \
         trigger, so this is the only detector"
    );
}

#[test]
fn a_trigger_naming_the_column_does_not_stop_the_drop() {
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch(
        "CREATE TABLE memory (id INTEGER PRIMARY KEY, encode_hash TEXT);
         CREATE TRIGGER t AFTER UPDATE OF encode_hash ON memory BEGIN SELECT 1; END;",
    )
    .expect("fixture");

    let sql = include_str!("../migrations/0016_drop_encode_hash.sql");
    conn.execute_batch(sql)
        .expect("sqlite drops the column even though a trigger names it — the silent case");

    let left: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 't'",
            [],
            |r| r.get(0),
        )
        .expect("trigger survived");
    assert!(
        left.contains("encode_hash"),
        "the trigger still names a column that no longer exists"
    );
}

#[test]
fn a_view_naming_the_column_stops_the_drop() {
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch(
        "CREATE TABLE memory (id INTEGER PRIMARY KEY, encode_hash TEXT);
         CREATE VIEW v AS SELECT encode_hash FROM memory;",
    )
    .expect("fixture");

    let sql = include_str!("../migrations/0016_drop_encode_hash.sql");
    let err = conn
        .execute_batch(sql)
        .expect_err("a view must refuse the drop");
    assert!(
        err.to_string().contains("encode_hash"),
        "the refusal names the culprit: {err}"
    );
}
