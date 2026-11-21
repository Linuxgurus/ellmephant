mod common;

use common::temp_store;
use ellmephant::store::audience::Reader;
use ellmephant::store::{self, Patch};

fn registry(conn: &rusqlite::Connection, name: &str) {
    conn.execute(
        "INSERT OR IGNORE INTO pen (name, kind) VALUES (?1, 'self')",
        rusqlite::params![name],
    )
    .expect("seed pen");
}

fn row(conn: &rusqlite::Connection, key: &str, pen: &str, audience: &str) {
    store::merge(
        conn,
        &Patch {
            key: Some(key.into()),
            title: Some(key.into()),
            body: Some("the facts".into()),
            acted_by: Some(pen.into()),
            asserted_by: Some(pen.into()),
            audience: Some(audience.into()),
            ..Default::default()
        },
    )
    .expect("write row");
}

fn visible(conn: &rusqlite::Connection, reader: &Reader) -> Vec<String> {
    let (sql, binds) = reader.sql();
    let mut stmt = conn
        .prepare(&format!("SELECT key FROM memory WHERE {sql} ORDER BY key"))
        .expect("prepare");
    stmt.query_map(rusqlite::params_from_iter(binds.iter()), |r| {
        r.get::<_, String>(0)
    })
    .expect("query")
    .map(Result::unwrap)
    .collect()
}

#[test]
fn a_pen_name_with_a_wildcard_matches_only_itself() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();

    for pen in ["under_score", "underXscore", "100%", "100X"] {
        registry(&conn, pen);
    }
    row(&conn, "mine-underscore", "under_score", "pen:under_score");
    row(&conn, "theirs-underscore", "underXscore", "pen:underXscore");
    row(&conn, "mine-subtree", "under_score", "subtree:under_score");
    row(
        &conn,
        "theirs-subtree",
        "underXscore",
        "subtree:underXscore",
    );
    row(&conn, "mine-percent", "100%", "pen:100%");
    row(&conn, "theirs-percent", "100X", "pen:100X");

    assert_eq!(
        visible(&conn, &Reader::pen("under_score").in_tree(&conn)),
        vec!["mine-subtree", "mine-underscore"],
        "`_` in a pen name is a character, not a wildcard"
    );
    assert_eq!(
        visible(&conn, &Reader::pen("underXscore").in_tree(&conn)),
        vec!["theirs-subtree", "theirs-underscore"],
        "and the other side of the pair sees only its own"
    );
    assert_eq!(
        visible(&conn, &Reader::pen("100%")),
        vec!["mine-percent"],
        "`%` in a pen name is a character, not a wildcard"
    );
    assert_eq!(visible(&conn, &Reader::pen("100X")), vec!["theirs-percent"]);
}
