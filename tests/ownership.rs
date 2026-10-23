//! Integration: OWNERSHIP is responsibility, not authorship (soma/ellmephant#109).
//!
//! `asserted_by` answers who typed it, `edited_by` who wrote the state being read; neither answers
//! the question a reader has — WHO IS ANSWERABLE. `owner` does, and it is nullable on purpose: NULL
//! means nobody is, the commons, where protected/private do not apply and anyone may adopt.

mod common;

use common::{count, temp_store};
use ellmephant::embed::HashEmbedder;
use ellmephant::store::{self, Memory};

const DIM: usize = 8;

fn registry(conn: &rusqlite::Connection, name: &str, parent: Option<&str>) {
    // A finger is registered as one. The name never decides it: a slash is a display convention, and
    // these tests exist partly to prove that renaming a pen does not change who owns its rows.
    let kind = if parent.is_some() && name.contains("finger") {
        "finger"
    } else {
        "self"
    };
    conn.execute(
        "INSERT OR IGNORE INTO pen (name, kind, parent_id)
         VALUES (?1, ?2, (SELECT id FROM pen WHERE name = ?3))",
        rusqlite::params![name, kind, parent],
    )
    .expect("seed pen");
}

fn row(conn: &rusqlite::Connection, embedder: &HashEmbedder, key: &str, author: &str) {
    let mem = Memory {
        key: key.to_string(),
        title: format!("written by {author}"),
        body: "the facts".to_string(),
        asserted_by: Some(author.to_string()),
        ..Default::default()
    };
    common::add_now(conn, embedder, &mem).expect("add memory");
}

fn owner(conn: &rusqlite::Connection, key: &str) -> Option<String> {
    store::get(conn, key).expect("get").expect("row").owner
}

#[test]
fn a_row_is_owned_by_its_author() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    registry(&conn, "stet", None);

    row(&conn, &embedder, "owned", "stet");
    assert_eq!(owner(&conn, "owned").as_deref(), Some("stet"));
}

#[test]
fn a_finger_never_owns_its_parent_does() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    registry(&conn, "stet", None);
    registry(&conn, "stet/finger-3", Some("stet"));

    row(&conn, &embedder, "by-a-finger", "stet/finger-3");
    assert_eq!(
        owner(&conn, "by-a-finger").as_deref(),
        Some("stet"),
        "the finger typed it; its parent is answerable"
    );
}

#[test]
fn disclaim_makes_a_row_commons_and_says_so() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    registry(&conn, "stet", None);
    row(&conn, &embedder, "released", "stet");

    // A PUBLIC row is released quietly: no publication to name.
    store::disclaim(&conn, "released", "stet", false).expect("disclaim");

    let mem = store::get(&conn, "released").unwrap().unwrap();
    assert_eq!(mem.owner, None, "nobody is answerable now");
    assert_eq!(
        mem.audience, "public",
        "a disclaimed row is public: protected/private do not apply to the commons"
    );

    let events = store::pen_events(&conn, "released").unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, "disclaim");
    assert_eq!(events[0].2.as_deref(), Some("stet"));
}

#[test]
fn anyone_may_adopt_a_commons_row_and_the_ledger_remembers() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    registry(&conn, "stet", None);
    registry(&conn, "cline", None);
    row(&conn, &embedder, "adopted", "stet");

    store::disclaim(&conn, "adopted", "stet", false).expect("disclaim");
    store::adopt(&conn, "adopted", "cline", false).expect("adopt");

    assert_eq!(owner(&conn, "adopted").as_deref(), Some("cline"));
    let events = store::pen_events(&conn, "adopted").unwrap();
    let actions: Vec<&str> = events.iter().map(|e| e.1.as_str()).collect();
    assert_eq!(
        actions,
        vec!["disclaim", "adopt"],
        "the ledger keeps the whole story"
    );
}

#[test]
fn a_finger_adopting_adopts_as_its_parent() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    registry(&conn, "stet", None);
    registry(&conn, "stet/finger-4", Some("stet"));
    row(&conn, &embedder, "adopted-by-finger", "stet");
    store::disclaim(&conn, "adopted-by-finger", "stet", false).unwrap();

    store::adopt(&conn, "adopted-by-finger", "stet/finger-4", false).expect("adopt");
    assert_eq!(owner(&conn, "adopted-by-finger").as_deref(), Some("stet"));
}

fn seed_pen(conn: &rusqlite::Connection, name: &str, parent: Option<&str>) {
    registry(conn, name, parent);
}

#[test]
fn inherit_moves_the_rows_to_the_parent() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet", None);
    seed_pen(&conn, "stet/finger-9", Some("stet"));
    // A finger's rows are already owned by its parent, so retire a CHILD pen that can own: use a
    // plain child pen, which the registry allows by name.
    seed_pen(&conn, "hand-of-stet", Some("stet"));
    row(&conn, &embedder, "hand-work", "hand-of-stet");

    let preview = store::retire_preview(&conn, "hand-of-stet").expect("preview");
    assert_eq!(preview.rows, 1);
    assert_eq!(preview.parent.as_deref(), Some("stet"));

    store::retire(
        &conn,
        "hand-of-stet",
        "stet",
        store::Rows::Inherit,
        store::History::Memorialize,
    )
    .expect("retire");

    assert_eq!(
        owner(&conn, "hand-work").as_deref(),
        Some("stet"),
        "the parent carries the work"
    );
    let events = store::pen_events(&conn, "hand-work").unwrap();
    assert_eq!(
        events[0].1, "transfer",
        "inheritance is a transfer, recorded per row"
    );
    let retired: Option<String> = conn
        .query_row(
            "SELECT ceased_at FROM pen WHERE name = 'hand-of-stet'",
            [],
            |r| r.get(0),
        )
        .expect("ceased_at");
    assert!(retired.is_some(), "a retired pen has ceased_at set");
}

#[test]
fn disclaim_on_retirement_publishes_private_rows() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet", None);
    seed_pen(&conn, "hand-of-stet", Some("stet"));
    row(&conn, &embedder, "secret-work", "hand-of-stet");
    conn.execute(
        "UPDATE memory SET audience = 'pen:hand-of-stet' WHERE key = 'secret-work'",
        [],
    )
    .unwrap();

    store::retire(
        &conn,
        "hand-of-stet",
        "stet",
        store::Rows::Disclaim,
        store::History::Memorialize,
    )
    .expect("retire");

    let mem = store::get(&conn, "secret-work").unwrap().unwrap();
    assert_eq!(mem.owner, None, "the commons: nobody is answerable");
    assert_eq!(
        mem.audience, "public",
        "a disclaimed private row is published"
    );
    assert_eq!(
        store::pen_events(&conn, "secret-work").unwrap()[0].1,
        "disclaim"
    );
}

#[test]
fn delete_on_retirement_removes_the_rows_and_can_purge_the_record() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet", None);
    seed_pen(&conn, "hand-of-stet", Some("stet"));
    row(&conn, &embedder, "doomed", "hand-of-stet");

    store::retire(
        &conn,
        "hand-of-stet",
        "stet",
        store::Rows::Delete,
        store::History::Purge,
    )
    .expect("retire");

    assert!(
        store::get(&conn, "doomed").unwrap().is_none(),
        "the row is gone"
    );
    let versions: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_version WHERE key = 'doomed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        versions, 0,
        "purge leaves no record — the store's only true destroyer"
    );
}

#[test]
fn a_root_pen_is_not_retirable_by_anyone() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    seed_pen(&conn, "estate", None);
    seed_pen(&conn, "james", Some("estate"));

    // Not even its own child, and not even with `inherit`: the root's standing changes by
    // configuration, not by a call.
    for caller in ["james", "estate"] {
        let err = store::retire(
            &conn,
            "estate",
            caller,
            store::Rows::Disclaim,
            store::History::Memorialize,
        )
        .expect_err("the root is not retirable");
        assert!(
            format!("{err}").contains("not retirable"),
            "the message says the root changes by configuration"
        );
    }
}

#[test]
fn a_pen_cannot_be_retired_twice() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    seed_pen(&conn, "stet", None);
    seed_pen(&conn, "hand-of-stet", Some("stet"));

    store::retire(
        &conn,
        "hand-of-stet",
        "stet",
        store::Rows::Disclaim,
        store::History::Memorialize,
    )
    .unwrap();
    let err = store::retire(
        &conn,
        "hand-of-stet",
        "stet",
        store::Rows::Disclaim,
        store::History::Memorialize,
    )
    .expect_err("already retired");
    assert!(format!("{err}").contains("already retired"));
}

#[test]
fn an_ancestor_may_disclaim_across_generations() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet", None);
    seed_pen(&conn, "stet/mid", Some("stet"));
    seed_pen(&conn, "stet/mid/leaf", Some("stet/mid"));
    row(&conn, &embedder, "deep-work", "stet/mid/leaf");

    // Grandparent, skipping a generation: allowed, and it is the same release.
    store::disclaim(&conn, "deep-work", "stet", false)
        .expect("an ancestor may disclaim a descendant's row");
    assert_eq!(owner(&conn, "deep-work"), None);
}

#[test]
fn a_sibling_may_not_adopt_an_owned_row_and_an_ancestor_must_force_it() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet", None);
    seed_pen(&conn, "hand-a", Some("stet"));
    seed_pen(&conn, "hand-b", Some("stet"));
    row(&conn, &embedder, "a-work", "hand-a");

    // A sibling is not an ancestor: refused before force is even considered.
    let err =
        store::adopt(&conn, "a-work", "hand-b", true).expect_err("siblings may not take rows");
    assert!(format!("{err}").contains("ancestor"));

    // The parent may, but only deliberately.
    let err = store::adopt(&conn, "a-work", "stet", false).expect_err("an owned row needs force");
    assert!(format!("{err}").contains("force=true"));

    store::adopt(&conn, "a-work", "stet", true).expect("an ancestor may force-take it");
    assert_eq!(owner(&conn, "a-work").as_deref(), Some("stet"));
}

#[test]
fn an_ancestor_may_not_publish_a_descendants_private_row_without_saying_so() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet", None);
    seed_pen(&conn, "stet/mid", Some("stet"));
    seed_pen(&conn, "stet/mid/leaf", Some("stet/mid"));
    row(&conn, &embedder, "private-work", "stet/mid/leaf");
    conn.execute(
        "UPDATE memory SET audience = 'pen:stet/mid/leaf' WHERE key = 'private-work'",
        [],
    )
    .unwrap();

    // Releasing it would publish another pen's words. Refused unless the act is named.
    let err = store::disclaim(&conn, "private-work", "stet", false)
        .expect_err("publishing needs the flag");
    assert!(format!("{err}").contains("publish=true"));

    // Named explicitly, it is allowed — and the ledger calls it a publish, not a disclaim.
    store::disclaim(&conn, "private-work", "stet", true).expect("publish deliberately");
    let mem = store::get(&conn, "private-work").unwrap().unwrap();
    assert_eq!(mem.audience, "public");
    assert_eq!(
        store::pen_events(&conn, "private-work").unwrap()[0].1,
        "publish"
    );
}

#[test]
fn renaming_a_pen_does_not_change_who_owns_its_rows() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet", None);
    seed_pen(&conn, "hand-a", Some("stet"));
    row(&conn, &embedder, "signed-work", "hand-a");

    conn.execute(
        "UPDATE pen SET name = 'renamed-hand' WHERE name = 'hand-a'",
        [],
    )
    .unwrap();

    // Ownership is a foreign key, not a name: the rename cannot move it, and the derived owner
    // display follows the new name because it is a join, not a copy.
    assert_eq!(owner(&conn, "signed-work").as_deref(), Some("renamed-hand"));
}

#[test]
fn a_name_with_a_slash_does_not_make_a_finger() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet", None);
    // Registered as a plain self whose NAME looks like a finger. The name must not decide kind.
    conn.execute(
        "INSERT INTO pen (name, kind, parent_id) VALUES ('stet/looks-like-a-finger', 'self', (SELECT id FROM pen WHERE name='stet'))",
        [],
    )
    .unwrap();

    row(&conn, &embedder, "its-own-work", "stet/looks-like-a-finger");
    assert_eq!(
        owner(&conn, "its-own-work").as_deref(),
        Some("stet/looks-like-a-finger"),
        "kind is the column; a slash is an address"
    );
}

#[test]
fn a_synthetic_snapshot_is_marked_and_excludable() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet", None);
    row(&conn, &embedder, "observed", "stet");

    // A version written by the migration carries synthetic = 1; an observed edit does not.
    conn.execute(
        "INSERT INTO memory_version (type, key, n, encoding, note, content, synthetic)
         VALUES ('memory', 'observed', 1, 'json', 'SYNTHETIC', '{}', 1)",
        [],
    )
    .unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM memory_version WHERE synthetic = 1"
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM memory_version WHERE synthetic = 0"
        ),
        0,
        "the migration's snapshot must not answer a query for observed edits"
    );
}
