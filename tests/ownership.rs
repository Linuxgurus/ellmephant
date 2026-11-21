mod common;

use common::{count, temp_store};

use ellmephant::embed::HashEmbedder;

use ellmephant::store::{self, Memory};

const DIM: usize = 8;

fn registry(conn: &rusqlite::Connection, name: &str, parent: Option<&str>) {
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

    let err =
        store::adopt(&conn, "a-work", "hand-b", true).expect_err("siblings may not take rows");
    assert!(format!("{err}").contains("ancestor"));

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

    let err = store::disclaim(&conn, "private-work", "stet", false)
        .expect_err("publishing needs the flag");
    assert!(format!("{err}").contains("publish=true"));

    let err = store::disclaim(&conn, "private-work", "stet", true)
        .expect_err("an ancestor may not publish what it cannot read");
    assert!(format!("{err}").contains("cannot publish"), "{err}");

    conn.execute(
        "UPDATE memory SET audience = 'pen:stet/mid/leaf,pen:stet' WHERE key = 'private-work'",
        [],
    )
    .unwrap();
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

    assert_eq!(owner(&conn, "signed-work").as_deref(), Some("renamed-hand"));
}

#[test]
fn a_name_with_a_slash_does_not_make_a_finger() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet", None);
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

fn subtree() -> (tempfile::TempDir, rusqlite::Connection) {
    let (dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).unwrap();
    seed_pen(&conn, "estate", None);
    seed_pen(&conn, "james", Some("estate"));
    seed_pen(&conn, "stet", Some("james"));
    seed_pen(&conn, "cline", Some("james"));
    (dir, conn)
}

fn rewrite(
    conn: &rusqlite::Connection,
    _embedder: &HashEmbedder,
    key: &str,
    author: &str,
) -> String {
    let patch = store::Patch {
        key: Some(key.to_string()),
        acted_by: Some(author.to_string()),
        title: Some(format!("rewritten by {author}")),
        body: Some("something else".to_string()),
        asserted_by: Some(author.to_string()),
        ..Default::default()
    };
    match store::merge(conn, &patch) {
        Ok(_) => String::new(),
        Err(e) => format!("{e}"),
    }
}

#[test]
fn a_peer_may_not_rewrite_another_pens_row() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-note", "stet");

    let err = rewrite(&conn, &embedder, "stets-note", "cline");
    assert!(
        err.contains("may not write") && err.contains("stet"),
        "a sibling is refused by name: {err}"
    );
    let mem = store::get(&conn, "stets-note").unwrap().unwrap();
    assert_eq!(
        mem.title, "written by stet",
        "the refused write changed nothing"
    );
    assert_eq!(mem.owner.as_deref(), Some("stet"));
}

#[test]
fn an_ancestor_may_rewrite_a_descendants_row_without_taking_it() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-note", "stet");

    let err = rewrite(&conn, &embedder, "stets-note", "james");
    assert!(err.is_empty(), "an ancestor reaches down: {err}");

    let mem = store::get(&conn, "stets-note").unwrap().unwrap();
    assert_eq!(mem.title, "rewritten by james");
    assert_eq!(
        mem.owner.as_deref(),
        Some("stet"),
        "editing is not adopting: responsibility stays where it was taken on"
    );
    assert_eq!(
        mem.edited_by.as_deref(),
        Some("james"),
        "and the edit is named"
    );
}

#[test]
fn the_commons_belongs_to_every_pen() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "everyones", "stet");
    store::disclaim(&conn, "everyones", "stet", false).expect("disclaim");

    let err = rewrite(&conn, &embedder, "everyone", "cline");
    assert!(
        err.contains("no such memory") || err.is_empty(),
        "a new key is anyone's: {err}"
    );
    let err = rewrite(&conn, &embedder, "everyones", "cline");
    assert!(
        err.is_empty(),
        "nobody owns the commons, so anyone may write it: {err}"
    );
    assert_eq!(
        owner(&conn, "everyones"),
        None,
        "and nobody becomes answerable by writing"
    );
}

#[test]
fn a_finger_writes_on_its_parents_rows() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    seed_pen(&conn, "stet/finger-7", Some("stet"));
    row(&conn, &embedder, "shared-work", "stet");

    let err = rewrite(&conn, &embedder, "shared-work", "stet/finger-7");
    assert!(err.is_empty(), "a finger acts as its parent: {err}");
    assert_eq!(owner(&conn, "shared-work").as_deref(), Some("stet"));
}

#[test]
fn a_peer_may_not_delete_another_pens_row() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-note", "stet");

    let err = store::delete(&conn, "stets-note", Some("cline")).expect_err("a sibling is refused");
    assert!(format!("{err}").contains("may not write"), "{err}");
    assert!(
        store::get(&conn, "stets-note").unwrap().is_some(),
        "a refused delete removes nothing"
    );
}

#[test]
fn deleting_leaves_a_named_ledger_entry() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-note", "stet");

    assert!(store::delete(&conn, "stets-note", Some("stet")).expect("owner may delete"));
    assert!(store::get(&conn, "stets-note").unwrap().is_none());

    let events = store::pen_events(&conn, "stets-note").unwrap();
    let delete = events
        .iter()
        .find(|e| e.1 == "delete")
        .expect("the ledger keeps what happened even though the row is gone");
    assert_eq!(delete.4.as_deref(), Some("stet"), "who removed it");
    assert_eq!(delete.2.as_deref(), Some("stet"), "what it was before");
    assert_eq!(
        delete.3, None,
        "and nothing took it on: a removal has no heir"
    );
}

#[test]
fn an_ancestor_may_delete_and_the_record_says_so() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-note", "stet");

    assert!(store::delete(&conn, "stets-note", Some("james")).expect("an ancestor reaches down"));
    let events = store::pen_events(&conn, "stets-note").unwrap();
    let delete = events
        .iter()
        .find(|e| e.1 == "delete")
        .expect("delete event");
    assert_eq!(
        delete.4.as_deref(),
        Some("james"),
        "the actor is the pen that did it, not the pen that owned it"
    );
    assert_eq!(delete.2.as_deref(), Some("stet"), "the row was stet's");
}

#[test]
fn a_commons_row_may_be_removed_by_anyone() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "everyones", "stet");
    store::disclaim(&conn, "everyones", "stet", false).expect("disclaim");

    assert!(store::delete(&conn, "everyones", Some("cline")).expect("the commons is anyone's"));
}

#[test]
fn stats_separate_the_commons_from_the_unattributed() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);

    row(&conn, &embedder, "released", "stet");
    store::disclaim(&conn, "released", "stet", false).expect("disclaim");

    conn.execute(
        "INSERT INTO memory (key, title, body, created, updated, audience)
         VALUES ('ghost', 'a ghost', 'no author', datetime('now'), datetime('now'), 'public')",
        [],
    )
    .unwrap();

    let stats = store::stats::compute(&conn).expect("stats");
    assert_eq!(stats.commons, 1, "one released row, author still named");
    assert_eq!(
        stats.unattributed, 1,
        "one row nobody wrote and nobody owns"
    );
}

#[test]
fn the_ownerless_rows_can_be_listed_not_only_counted() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);

    row(&conn, &embedder, "released", "stet");
    store::disclaim(&conn, "released", "stet", false).expect("disclaim");

    conn.execute(
        "INSERT INTO memory (key, title, body, created, updated, audience)
         VALUES ('unwitnessed', 't', 'b', datetime('now'), datetime('now'), 'public')",
        [],
    )
    .unwrap();

    let rows = store::unowned(&conn, &store::audience::Reader::everything(), 50).expect("list");
    assert_eq!(rows.len(), 2);
    let by_key: std::collections::HashMap<_, _> =
        rows.iter().map(|r| (r.key.as_str(), r)).collect();

    let released = by_key["released"];
    assert_eq!(
        released.why, "disclaimed",
        "released by a pen, author still named"
    );
    assert_eq!(released.asserted_by, "stet");

    let unwitnessed = by_key["unwitnessed"];
    assert_eq!(
        unwitnessed.why, "unrecorded",
        "no currently existing pen wrote it"
    );
    assert_eq!(unwitnessed.asserted_by, "anonymous");

    assert!(
        rows.iter()
            .all(|r| !r.key.is_empty() && !r.created.is_empty())
    );
}

#[test]
fn the_unowned_listing_obeys_the_audience_tier() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);

    row(&conn, &embedder, "released", "stet");
    store::disclaim(&conn, "released", "stet", false).expect("disclaim");

    row(&conn, &embedder, "secret", "stet");
    conn.execute(
        "UPDATE memory SET audience = 'pen:stet', owner = NULL WHERE key = 'secret'",
        [],
    )
    .unwrap();

    let anon = store::unowned(&conn, &store::audience::Reader::anonymous(), 50).expect("list");
    let keys: Vec<&str> = anon.iter().map(|r| r.key.as_str()).collect();
    assert!(
        keys.contains(&"released"),
        "the commons is anybody's: {keys:?}"
    );
    assert!(
        !keys.contains(&"secret"),
        "a private row's ADDRESS is not the commons: {keys:?}"
    );

    let owner = store::unowned(&conn, &store::audience::Reader::pen("stet"), 50).expect("list");
    let keys: Vec<&str> = owner.iter().map(|r| r.key.as_str()).collect();
    assert!(keys.contains(&"secret"), "its own pen may see it: {keys:?}");
}

#[test]
fn destroying_an_invisible_private_row_says_forbidden_not_absent() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-secret", "stet");
    conn.execute(
        "UPDATE memory SET audience = 'pen:stet' WHERE key = 'stets-secret'",
        [],
    )
    .unwrap();

    let read = store::get(&conn, "stets-secret").unwrap();
    assert!(
        read.is_some(),
        "the store itself can read it; the reader tier is what hides it"
    );

    let err = store::delete(&conn, "stets-secret", Some("cline")).unwrap_err();
    assert!(
        format!("{err}").contains("forbidden"),
        "a write refusal is FORBIDDEN, not a claim of absence: {err}"
    );
    assert!(store::get(&conn, "stets-secret").unwrap().is_some());

    assert!(store::delete(&conn, "stets-secret", Some("stet")).is_ok());
}

#[test]
fn a_merge_is_a_delete_and_is_gated_like_one() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-note", "stet");
    row(&conn, &embedder, "clines-note", "cline");
    row(&conn, &embedder, "stets-other", "stet");

    let err =
        store::links::merge_into(&conn, "stets-note", "clines-note", Some("cline")).unwrap_err();
    assert!(
        format!("{err}").contains("may not write") || format!("{err}").contains("no such memory"),
        "a merge is a delete and needs the same reach: {err}"
    );
    assert!(
        store::get(&conn, "stets-note").unwrap().is_some(),
        "the source survives"
    );

    let err =
        store::links::merge_into(&conn, "stets-note", "clines-note", Some("stet")).unwrap_err();
    assert!(
        format!("{err}").contains("may not write"),
        "the survivor is somebody else's: {err}"
    );
    assert!(store::links::merge_into(&conn, "stets-note", "stets-other", Some("stet")).is_ok());
}

#[test]
fn a_pen_mints_a_child_and_the_parent_is_the_caller() {
    let (_dir, conn) = subtree();
    let minted = store::mint(&conn, "stet", "stet/hand", "self").expect("mint");

    assert_eq!(minted.parent, "stet", "the caller is the parent, by right");
    assert_eq!(minted.kind, "self");
    let parent: Option<String> = conn
        .query_row(
            "SELECT p.name FROM pen c JOIN pen p ON p.id = c.parent_id WHERE c.name = 'stet/hand'",
            [],
            |r| r.get(0),
        )
        .expect("parent");
    assert_eq!(parent.as_deref(), Some("stet"));

    store::mint(&conn, "stet/hand", "stet/hand/finger-9", "finger").expect("mint a finger");
}

#[test]
fn a_kind_is_never_inferred_from_the_name() {
    let (_dir, conn) = subtree();
    let minted = store::mint(&conn, "stet", "assistant", "finger").expect("mint");
    assert_eq!(minted.kind, "finger");
    let err = store::mint(&conn, "stet", "weird-kind", "hand").unwrap_err();
    assert!(format!("{err}").contains("`self` or `finger`"), "{err}");
}

#[test]
fn a_name_is_spent_and_a_retired_pen_cannot_mint() {
    let (_dir, conn) = subtree();
    store::mint(&conn, "stet", "stet/old-hand", "self").expect("mint");

    let err = store::mint(&conn, "stet", "stet/old-hand", "self").unwrap_err();
    assert!(format!("{err}").contains("already a pen"), "{err}");

    store::retire(
        &conn,
        "stet/old-hand",
        "stet",
        store::Rows::Disclaim,
        store::History::Memorialize,
    )
    .expect("retire");
    let err = store::mint(&conn, "stet", "stet/old-hand", "self").unwrap_err();
    assert!(format!("{err}").contains("a name is spent"), "{err}");

    let err = store::mint(&conn, "stet/old-hand", "stet/old-hand/child", "finger").unwrap_err();
    assert!(format!("{err}").contains("has been retired"), "{err}");
}

#[test]
fn a_hand_may_credit_its_finger_and_nothing_else() {
    let (_dir, conn) = subtree();
    store::mint(&conn, "stet", "stet/hand", "self").expect("mint");
    store::mint(&conn, "stet/hand", "stet/hand/finger-1", "finger").expect("mint");

    store::may_act_as(&conn, "stet", "stet").expect("itself");

    let err = store::may_act_as(&conn, "stet/hand", "stet/hand/finger-1").unwrap_err();
    assert!(format!("{err}").contains("LIVE lend"), "{err}");

    let lent = store::sudo::lend(&conn, "stet/hand", "stet/hand/finger-1", 60).expect("lend");
    assert_eq!(lent.powerful, "stet/hand");
    assert_eq!(lent.weak, "stet/hand/finger-1");
    store::may_act_as(&conn, "stet/hand", "stet/hand/finger-1")
        .expect("its own finger, while lent");
    store::sudo::lend(&conn, "stet", "stet/hand/finger-1", 60).expect("lend down the subtree");
    store::may_act_as(&conn, "stet", "stet/hand/finger-1")
        .expect("down the whole subtree, while lent");

    store::sudo::release(&conn, "stet/hand/finger-1", "stet/hand").expect("release");
    let err = store::may_act_as(&conn, "stet/hand", "stet/hand/finger-1").unwrap_err();
    assert!(
        format!("{err}").contains("LIVE lend"),
        "revocation is immediate: {err}"
    );

    conn.execute(
        "UPDATE active_sudo SET expires_at = datetime('now', '-1 second')",
        [],
    )
    .unwrap();
    let err = store::may_act_as(&conn, "stet", "stet/hand/finger-1").unwrap_err();
    assert!(
        format!("{err}").contains("LIVE lend"),
        "expiry is immediate: {err}"
    );

    let err = store::may_act_as(&conn, "stet", "cline").unwrap_err();
    assert!(format!("{err}").contains("never a peer"), "{err}");
    let err = store::may_act_as(&conn, "stet/hand", "stet").unwrap_err();
    assert!(format!("{err}").contains("never a peer"), "{err}");
    let err = store::may_act_as(&conn, "stet", "nobody-at-all").unwrap_err();
    assert!(format!("{err}").contains("is not a pen"), "{err}");
}

#[test]
fn a_fingers_work_is_its_hands_and_retirement_moves_it() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    store::mint(&conn, "stet", "stet/hand", "self").expect("mint");
    store::mint(&conn, "stet/hand", "stet/hand/finger-2", "finger").expect("mint");

    store::sudo::lend(&conn, "stet/hand", "stet/hand/finger-2", 300).expect("lend");
    store::may_act_as(&conn, "stet/hand", "stet/hand/finger-2").expect("act as the finger");
    row(&conn, &embedder, "finger-work", "stet/hand/finger-2");
    assert_eq!(owner(&conn, "finger-work").as_deref(), Some("stet/hand"));

    store::retire(
        &conn,
        "stet/hand/finger-2",
        "stet/hand",
        store::Rows::Inherit,
        store::History::Memorialize,
    )
    .expect("dispose of the finger");
    assert_eq!(owner(&conn, "finger-work").as_deref(), Some("stet/hand"));
    assert!(store::pen_events(&conn, "finger-work").unwrap().is_empty());
    let ceased: Option<String> = conn
        .query_row(
            "SELECT ceased_at FROM pen WHERE name = 'stet/hand/finger-2'",
            [],
            |r| r.get(0),
        )
        .expect("ceased_at");
    assert!(
        ceased.is_some(),
        "the pen is disposed of; the work stays where it was"
    );
}

#[test]
fn a_pen_cannot_be_named_anonymous_or_given_an_unaddressable_name() {
    let (_dir, conn) = subtree();

    for name in ["anonymous", "ANONYMOUS", " anonymous "] {
        let err = store::mint(&conn, "stet", name, "finger").unwrap_err();
        assert!(format!("{err}").contains("reserved"), "{name}: {err}");
    }

    for name in ["stet/finger,1", "stet finger"] {
        let err = store::mint(&conn, "stet", name, "finger").unwrap_err();
        assert!(format!("{err}").contains("addressed"), "{name}: {err}");
    }

    store::mint(&conn, "stet", "stet/finger-9f3a1c", "finger").expect("a good name");
}

fn deeper_tree(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "DELETE FROM pen;
         INSERT INTO pen (name, kind) VALUES ('estate', 'self');
         INSERT INTO pen (name, kind, parent_id) VALUES ('james', 'self', (SELECT id FROM pen WHERE name='estate'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('stet', 'self', (SELECT id FROM pen WHERE name='james'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('cline', 'self', (SELECT id FROM pen WHERE name='james'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('stet/hand', 'self', (SELECT id FROM pen WHERE name='stet'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('stet/hand/f-1', 'finger', (SELECT id FROM pen WHERE name='stet/hand'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('stet/hand/f-1/f-2', 'finger', (SELECT id FROM pen WHERE name='stet/hand/f-1'));
         INSERT INTO pen (name, kind, parent_id) VALUES ('island', 'self', NULL);
         INSERT INTO pen (name, kind, parent_id) VALUES ('island/x', 'self', (SELECT id FROM pen WHERE name='island'));",
    )
    .expect("pen tree");
}

#[test]
fn a_walk_names_every_relation_and_never_doubles_back() {
    let (_dir, conn) = subtree();
    deeper_tree(&conn);

    let kin = |a: &str, b: &str| {
        store::relation::relation(&conn, a, b)
            .unwrap()
            .map(|k| k.gloss())
    };

    assert_eq!(kin("stet", "stet").as_deref(), Some("ME"));
    assert_eq!(kin("stet", "james").as_deref(), Some("P \"parent\""));
    assert_eq!(kin("stet", "estate").as_deref(), Some("PP \"grandparent\""));
    assert_eq!(kin("james", "stet").as_deref(), Some("C \"child\""));
    assert_eq!(kin("estate", "stet").as_deref(), Some("CC \"grandchild\""));
    assert_eq!(kin("stet", "cline").as_deref(), Some("S \"sibling\""));
    assert_eq!(kin("stet", "stet/hand").as_deref(), Some("C \"child\""));
    assert_eq!(
        kin("stet", "stet/hand/f-1").as_deref(),
        Some("CC \"grandchild\"")
    );
    assert_eq!(
        kin("stet/hand/f-1", "stet/hand/f-1/f-2").as_deref(),
        Some("C \"child\"")
    );
    assert_eq!(
        kin("stet/hand", "stet/hand/f-1/f-2").as_deref(),
        Some("CC \"grandchild\"")
    );

    assert_eq!(kin("stet/hand", "cline").as_deref(), Some("PS \"uncle\""));
    assert_eq!(kin("cline", "stet/hand").as_deref(), Some("SC \"nephew\""));
    assert_eq!(
        kin("stet/hand", "island").as_deref(),
        None,
        "no common root"
    );

    let sibling = store::relation::relation(&conn, "stet", "cline")
        .unwrap()
        .unwrap();
    assert_eq!(sibling.walk, "S");
    assert!(!sibling.walk.contains("PC"));
}

#[test]
fn a_cousin_is_the_walk_through_the_common_ancestor() {
    let (_dir, conn) = subtree();
    deeper_tree(&conn);

    conn.execute(
        "INSERT INTO pen (name, kind, parent_id) VALUES ('cline/hand', 'self', (SELECT id FROM pen WHERE name='cline'))",
        [],
    )
    .unwrap();

    let kin = |a: &str, b: &str| store::relation::relation(&conn, a, b).unwrap().unwrap();
    assert_eq!(kin("stet/hand", "cline/hand").walk, "PSC");
    assert_eq!(kin("stet/hand", "cline/hand").word, Some("cousin"));
    let removed = kin("stet/hand/f-1/f-2", "cline/hand");
    assert_eq!(removed.walk, "PPPSC");
    assert_eq!(removed.word, None);
}

#[test]
fn whois_answers_what_a_pen_is_and_what_it_is_to_you() {
    let (_dir, conn) = subtree();
    deeper_tree(&conn);
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "hands-work", "stet/hand");

    let seen = store::whois(&conn, "stet", "stet/hand").expect("whois");
    assert_eq!(seen.name, "stet/hand");
    assert_eq!(seen.kind, "self");
    assert_eq!(seen.parent.as_deref(), Some("stet"));
    assert_eq!(
        seen.relation.as_deref(),
        Some("C \"child\""),
        "what it is to me"
    );
    assert_eq!(seen.owns, 1, "the rows it answers for");
    assert!(seen.ceased.is_none());

    let from_cline = store::whois(&conn, "cline", "stet/hand").expect("whois");
    assert_eq!(from_cline.relation.as_deref(), Some("SC \"nephew\""));
    let from_self = store::whois(&conn, "stet/hand", "stet/hand").expect("whois");
    assert_eq!(from_self.relation.as_deref(), Some("ME"));

    let island = store::whois(&conn, "stet", "island").expect("whois");
    assert_eq!(
        island.relation, None,
        "no common root is an answer, not an error"
    );

    store::retire(
        &conn,
        "stet/hand",
        "stet",
        store::Rows::Disclaim,
        store::History::Memorialize,
    )
    .expect("retire");
    let gone = store::whois(&conn, "stet", "stet/hand").expect("whois");
    assert!(gone.ceased.is_some(), "history outlives the thing");

    let err = store::whois(&conn, "stet", "nobody-at-all").unwrap_err();
    assert!(format!("{err}").contains("no pen named"), "{err}");
}

#[test]
fn an_ancestor_writes_a_descendants_row_and_the_attribution_says_so() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-note", "stet");

    let rewrite = store::Patch {
        key: Some("stets-note".into()),
        acted_by: Some("james".into()),
        title: Some("rewritten by james".into()),
        body: Some("the ancestor's correction".into()),
        asserted_by: Some("james".into()),
        ..Default::default()
    };
    store::merge(&conn, &rewrite).expect("an ancestor may write down its subtree");

    let mem = store::get(&conn, "stets-note").unwrap().unwrap();
    assert_eq!(
        mem.asserted_by.as_deref(),
        Some("stet"),
        "the author does not move: authorship is write-once"
    );
    assert_eq!(
        mem.edited_by.as_deref(),
        Some("james"),
        "the ancestor is recorded as the writer, and the author does not move"
    );
    assert_eq!(
        mem.owner.as_deref(),
        Some("stet"),
        "responsibility did not move"
    );
}

#[test]
fn the_tree_attaches_from_configuration_and_becomes_one_root() {
    let (_dir, conn) = subtree();
    conn.execute("UPDATE pen SET parent_id = NULL", []).unwrap();
    let roots = |c: &rusqlite::Connection| -> i64 {
        c.query_row(
            "SELECT count(*) FROM pen WHERE parent_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(roots(&conn), 4, "five names, all roots");

    store::set_parent(&conn, "james", "estate").expect("james under estate");
    store::set_parent(&conn, "stet", "james").expect("stet under james");
    store::set_parent(&conn, "cline", "james").expect("cline under james");

    assert_eq!(roots(&conn), 1, "one root: the estate");
    assert!(
        store::relation::relation(&conn, "estate", "cline")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store::relation::relation(&conn, "stet", "cline")
            .unwrap()
            .unwrap()
            .walk,
        "S"
    );
}

#[test]
fn a_bad_edge_is_refused_by_name_and_the_rest_apply() {
    let (_dir, conn) = subtree();
    conn.execute("UPDATE pen SET parent_id = NULL", []).unwrap();

    store::set_parent(&conn, "james", "estate").unwrap();
    let err = store::set_parent(&conn, "estate", "james").unwrap_err();
    assert!(format!("{err}").contains("loop"), "{err}");
    let err = store::set_parent(&conn, "stet", "stet").unwrap_err();
    assert!(format!("{err}").contains("own parent"), "{err}");
    let err = store::set_parent(&conn, "stet", "nobody-at-all").unwrap_err();
    assert!(format!("{err}").contains("no pen named"), "{err}");

    store::set_parent(&conn, "stet", "james").unwrap();
    store::set_parent(&conn, "stet", "james").unwrap();
    let parent: Option<String> = conn
        .query_row(
            "SELECT p.name FROM pen c JOIN pen p ON p.id = c.parent_id WHERE c.name = 'stet'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(parent.as_deref(), Some("james"));
}

#[test]
fn a_seed_never_overwrites_what_the_table_already_says() {
    let (_dir, conn) = subtree();
    store::set_parent(&conn, "stet", "estate").unwrap();

    let seeded = store::seed_parent(&conn, "stet", "james").unwrap();
    assert_eq!(
        seeded,
        store::Seeded::Disagrees("estate".into()),
        "{seeded:?}"
    );

    let parent: Option<String> = conn
        .query_row(
            "SELECT p.name FROM pen c JOIN pen p ON p.id = c.parent_id WHERE c.name = 'stet'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        parent.as_deref(),
        Some("estate"),
        "the TABLE wins, not the config"
    );
}

#[test]
fn a_seed_attaches_a_bare_pen_and_agrees_when_repeated() {
    let (_dir, conn) = subtree();
    conn.execute("UPDATE pen SET parent_id = NULL", []).unwrap();
    assert_eq!(
        store::seed_parent(&conn, "james", "estate").unwrap(),
        store::Seeded::Attached
    );
    assert_eq!(
        store::seed_parent(&conn, "james", "estate").unwrap(),
        store::Seeded::Already
    );
}

#[test]
fn a_second_pen_differing_only_in_case_cannot_be_minted() {
    let (_dir, conn) = subtree();
    let err = store::mint(&conn, "estate", "JAMES", "self").unwrap_err();
    assert!(
        format!("{err}").to_lowercase().contains("case"),
        "the refusal should NAME the reason, not be a bare constraint error: {err}"
    );
}

#[test]
fn lookups_fold_case_and_the_spelling_is_preserved() {
    let (_dir, conn) = subtree();
    registry(&conn, "MixedCase", None);
    let preview = store::retire_preview(&conn, "mixedcase").expect("a folded lookup finds the row");
    assert_eq!(preview.pen, "MixedCase", "and reports the stored spelling");
    let spelling: String = conn
        .query_row(
            "SELECT name FROM pen WHERE lower(name) = 'mixedcase'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(spelling, "MixedCase", "the stored spelling is untouched");
}

#[test]
fn the_case_folded_index_is_built_once_the_table_is_clean() {
    let (_dir, conn) = subtree();
    store::ensure_name_uniqueness(&conn).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'pen_name_nocase'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "the index exists on a clean table");
    let bad = conn.execute(
        "INSERT INTO pen (name, kind, created) VALUES ('STET', 'self', datetime('now'))",
        [],
    );
    assert!(
        bad.is_err(),
        "a case-colliding INSERT must be refused by the trigger"
    );
}

#[test]
fn holding_a_session_reaches_the_powerful_pens_rows_and_stops_when_released() {
    let (_dir, conn) = subtree();
    store::set_parent(&conn, "stet", "estate").unwrap();
    let embedder = HashEmbedder::new(DIM);
    registry(&conn, "stets-hand", Some("stet"));
    row(&conn, &embedder, "stets-private", "stets-hand");
    conn.execute(
        "UPDATE memory SET audience = 'pen:stets-hand',
                           owner = (SELECT id FROM pen WHERE name = 'stets-hand')
          WHERE key = 'stets-private'",
        [],
    )
    .unwrap();

    let sees = |who: &str| {
        ellmephant::store::audience::Reader::pen(who)
            .in_tree(&conn)
            .with_borrowed(&conn)
            .sees("pen:stets-hand")
    };
    assert!(!sees("cline"), "with no session, a nephew is refused");
    assert!(
        !sees("james"),
        "and so is an uncle-of-nobody: james is not an ancestor of stet"
    );

    store::sudo::grant(&conn, "estate", "james", 60, "test").unwrap();
    store::sudo::sudo(&conn, "james", "estate", 30).unwrap();
    assert!(sees("james"), "holding the estate reaches every branch");

    store::sudo::release(&conn, "james", "estate").unwrap();
    assert!(!sees("james"), "released means released");
}

#[test]
fn a_lend_dies_when_the_parentage_that_justified_it_moves() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    registry(&conn, "james-secret-hand", Some("james"));
    row(&conn, &embedder, "james-private", "james");
    conn.execute(
        "UPDATE memory SET audience = 'pen:james' WHERE key = 'james-private'",
        [],
    )
    .unwrap();

    let sees = |who: &str| {
        ellmephant::store::audience::Reader::pen(who)
            .in_tree(&conn)
            .with_borrowed(&conn)
            .sees("pen:james")
    };
    assert!(
        !sees("cline"),
        "no session: a child does not read its parent's rows"
    );

    store::sudo::lend(&conn, "james", "cline", 600).unwrap();
    assert!(sees("cline"), "the lend is live");

    store::set_parent(&conn, "cline", "estate").unwrap();
    assert!(
        !sees("cline"),
        "a lend whose parentage has moved grants nothing, even before it expires"
    );
}

#[test]
fn a_borrowed_read_is_recorded_as_borrowed_not_as_a_subtree_read() {
    let (_dir, conn) = subtree();
    store::set_parent(&conn, "stet", "estate").unwrap();
    store::sudo::grant(&conn, "estate", "james", 60, "test").unwrap();
    store::sudo::sudo(&conn, "james", "estate", 30).unwrap();

    let reader = ellmephant::store::audience::Reader::pen("james")
        .in_tree(&conn)
        .with_borrowed(&conn);
    assert!(
        reader.is_borrowed("pen:stet"),
        "stet's rows are reached by SPENDING a session, not by ancestry"
    );
    assert!(
        !reader.is_borrowed("subtree:james"),
        "a share aimed at the reader's own branch is a subtree read and must stay labelled so"
    );
    store::audit::record(&conn, "james", Some("k"), store::audit::Why::Borrowed, None);
    let recent = store::audit::recent(&conn, 1).unwrap();
    assert_eq!(recent[0].3, "borrowed");
}

#[test]
fn sessions_are_listable_and_expiry_is_visible() {
    let (_dir, conn) = subtree();
    store::sudo::grant(&conn, "estate", "james", 60, "test").unwrap();
    assert!(store::sudo::live_sessions(&conn, None).unwrap().is_empty());

    store::sudo::sudo(&conn, "james", "estate", 30).unwrap();
    store::sudo::lend(&conn, "james", "cline", 60).unwrap();

    let all = store::sudo::live_sessions(&conn, None).unwrap();
    assert_eq!(all.len(), 2, "both sessions are listed");
    assert!(
        all.iter()
            .any(|s| s.weak == "james" && s.powerful == "estate")
    );
    assert!(all.iter().any(|s| s.weak == "cline" && s.source == "lend"));

    let one = store::sudo::live_sessions(&conn, Some("cline")).unwrap();
    assert_eq!(one.len(), 1, "narrowed to one pen");

    store::sudo::release(&conn, "james", "estate").unwrap();
    let after = store::sudo::live_sessions(&conn, None).unwrap();
    assert_eq!(after.len(), 1, "a released session is gone from the list");
}

fn planted(
    conn: &rusqlite::Connection,
    embedder: &HashEmbedder,
    key: &str,
    author: &str,
    audience: &str,
) {
    row_with_audience(conn, embedder, key, author, "public");
    conn.execute(
        "UPDATE memory SET audience = ?1 WHERE key = ?2",
        rusqlite::params![audience, key],
    )
    .expect("plant the audience a legacy row could carry");
}

fn shared(conn: &rusqlite::Connection, embedder: &HashEmbedder, key: &str, audience: &str) {
    planted(conn, embedder, key, "james", audience);
}

fn sees(conn: &rusqlite::Connection, who: &str, key: &str) -> bool {
    let mem = store::get(conn, key).unwrap().unwrap();
    ellmephant::store::audience::Reader::pen(who)
        .in_tree(conn)
        .with_borrowed(conn)
        .sees(&mem.audience)
}

#[test]
fn a_row_shared_to_two_pens_is_readable_by_those_two_and_no_others() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    store::set_parent(&conn, "stet", "estate").unwrap();
    shared(&conn, &embedder, "tax", "pen:james,pen:cline");

    assert!(sees(&conn, "james", "tax"), "the writer");
    assert!(sees(&conn, "cline", "tax"), "the one it was shared with");
    assert!(!sees(&conn, "stet", "tax"), "a sibling is not on the list");
    assert!(
        !sees(&conn, "estate", "tax"),
        "and NOT an ancestor: `pen:` is exact now — ask for it by name"
    );
}

#[test]
fn a_subtree_share_reaches_the_branch_and_an_exact_share_does_not_reach_up() {
    let (_dir, conn) = subtree();
    store::set_parent(&conn, "stet", "estate").unwrap();
    let embedder = HashEmbedder::new(DIM);
    shared(&conn, &embedder, "branch", "subtree:james");
    shared(&conn, &embedder, "exact", "pen:cline");

    assert!(sees(&conn, "james", "branch"), "the branch's head");
    assert!(sees(&conn, "cline", "branch"), "and everything below it");
    assert!(
        !sees(&conn, "stet", "branch"),
        "another branch is not this subtree"
    );

    assert!(sees(&conn, "cline", "exact"), "addressed exactly to cline");
    assert!(
        !sees(&conn, "james", "exact"),
        "and NOT to the pen above it — that is the law he chose"
    );
}

#[test]
fn an_audience_naming_a_pen_that_does_not_exist_is_refused() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    let mem = Memory {
        key: "ghost".into(),
        title: "ghost".into(),
        body: "addressed to nobody".into(),
        asserted_by: Some("james".into()),
        audience: "pen:nobody-at-all".into(),
        ..Default::default()
    };
    let err = common::add_now(&conn, &embedder, &mem).unwrap_err();
    assert!(
        format!("{err}").contains("names no pen"),
        "a row nobody can read must be refused at write, not stored: {err}"
    );
}

#[test]
fn assignment_follows_the_tree_and_needs_force_to_take_it_back() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    registry(&conn, "a", None);
    registry(&conn, "a-child", Some("a"));
    registry(&conn, "peer", None);
    row(&conn, &embedder, "given", "a");
    store::disclaim(&conn, "given", "a", false).expect("disclaim");

    store::assign(&conn, "given", "a", "a-child", false).expect("downward assignment");
    assert_eq!(owner(&conn, "given").as_deref(), Some("a-child"));

    store::assign(&conn, "given", "a", "a-child", false).expect("idempotent");

    let err = store::assign(&conn, "given", "a", "a", false).unwrap_err();
    assert!(err.to_string().contains("force=true"), "{err}");
    store::assign(&conn, "given", "a", "a", true).expect("forced take-back");
    assert_eq!(owner(&conn, "given").as_deref(), Some("a"));

    conn.execute("UPDATE memory SET owner = NULL WHERE key = 'given'", [])
        .expect("disclaim again");
    let err = store::assign(&conn, "given", "a", "peer", false).unwrap_err();
    assert!(err.to_string().contains("own subtree"), "{err}");
}

fn row_with_audience(
    conn: &rusqlite::Connection,
    embedder: &HashEmbedder,
    key: &str,
    author: &str,
    audience: &str,
) {
    let mem = Memory {
        key: key.to_string(),
        title: format!("written by {author}"),
        body: "the facts".to_string(),
        asserted_by: Some(author.to_string()),
        audience: audience.to_string(),
        ..Default::default()
    };
    common::add_now(conn, embedder, &mem).expect("write the row");
}

fn merge_as(conn: &rusqlite::Connection, key: &str, actor: &str, audience: Option<&str>) -> String {
    let patch = store::Patch {
        key: Some(key.to_string()),
        acted_by: Some(actor.to_string()),
        body: Some(format!("changed by {actor}")),
        asserted_by: Some(actor.to_string()),
        audience: audience.map(|a| a.to_string()),
        ..Default::default()
    };
    match store::merge(conn, &patch) {
        Ok(_) => String::new(),
        Err(e) => format!("{e}"),
    }
}

#[test]
fn a_pen_may_not_overwrite_a_row_it_cannot_read() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "james-only", "james", "pen:james");

    let err = merge_as(&conn, "james-only", "cline", None);
    assert!(
        err.contains("may not write"),
        "a row you cannot read is not a row you may write: {err}"
    );
    let mem = store::get(&conn, "james-only").unwrap().unwrap();
    assert_eq!(
        mem.body, "the facts",
        "the refused write changed nothing — this was the whole finding"
    );
    assert_eq!(mem.asserted_by.as_deref(), Some("james"));
    assert_eq!(
        mem.edited_by.as_deref(),
        Some("james"),
        "no writer was recorded"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM memory_version"),
        0,
        "a refused write creates no version"
    );
}

#[test]
fn an_owner_may_repair_its_own_row_whose_audience_excludes_it() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    planted(&conn, &embedder, "trap", "cline", "pen:james");

    let err = merge_as(&conn, "trap", "cline", Some("pen:james,pen:cline"));
    assert!(
        err.is_empty(),
        "the owner may narrow its own exposure: {err}"
    );

    let mem = store::get(&conn, "trap").unwrap().expect("readable again");
    assert_eq!(
        mem.asserted_by.as_deref(),
        Some("cline"),
        "authorship is write-once and the repair is still cline's"
    );
    assert_eq!(mem.body, "changed by cline");
}

#[test]
fn an_ancestor_may_repair_a_descendants_trapped_row() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    planted(&conn, &embedder, "stets-trap", "stet", "pen:james");

    let err = merge_as(&conn, "stets-trap", "james", Some("pen:james,pen:stet"));
    assert!(
        err.is_empty(),
        "answering for the row means being able to fix it: {err}"
    );

    let err = merge_as(&conn, "stets-trap", "cline", None);
    assert!(
        err.contains("may not write"),
        "a sibling is not an owner and not an ancestor: {err}"
    );
}

#[test]
fn an_ancestor_may_not_overwrite_a_row_it_cannot_read() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "stets-private", "stet", "pen:stet");

    let err = merge_as(&conn, "stets-private", "james", None);
    assert!(
        err.contains("audience does not include you"),
        "an ancestor that cannot read it may not rewrite it: {err}"
    );
    let mem = store::get(&conn, "stets-private").unwrap().unwrap();
    assert_eq!(mem.body, "the facts", "nothing changed");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM memory_version"),
        0,
        "no version was created"
    );
}

#[test]
fn a_write_credited_to_a_finger_needs_a_live_lend_and_records_the_hand_as_the_writer() {
    let (_dir, conn) = subtree();
    let _embedder = HashEmbedder::new(DIM);
    store::mint(&conn, "stet", "stet/hand", "self").expect("mint");
    store::mint(&conn, "stet/hand", "stet/hand/finger-1", "finger").expect("mint");

    let patch = || store::Patch {
        key: Some("credited".into()),
        title: Some("credited".into()),
        body: Some("the finger's work".into()),
        asserted_by: Some("stet/hand/finger-1".into()),
        acted_by: Some("stet/hand".into()),
        ..Default::default()
    };

    let err = store::merge(&conn, &patch()).unwrap_err();
    assert!(
        format!("{err}").contains("LIVE lend"),
        "a descendant is necessary, a live lend is what permits: {err}"
    );

    store::sudo::lend(&conn, "stet/hand", "stet/hand/finger-1", 60).expect("lend");
    store::merge(&conn, &patch()).expect("credited write");

    let mem = store::get(&conn, "credited").unwrap().unwrap();
    assert_eq!(
        mem.asserted_by.as_deref(),
        Some("stet/hand/finger-1"),
        "the credit names the finger"
    );
    assert_eq!(
        mem.edited_by.as_deref(),
        Some("stet/hand"),
        "the writer names the hand that actually wrote it"
    );
}

#[test]
fn a_cas_compare_must_not_leak_a_version_to_a_pen_that_may_not_write() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "james-secret", "james", "pen:james");

    let patch = store::Patch {
        key: Some("james-secret".into()),
        body: Some("probe".into()),
        asserted_by: Some("cline".into()),
        acted_by: Some("cline".into()),
        expected_version: Some(0),
        ..Default::default()
    };
    let err = format!("{}", store::merge(&conn, &patch).unwrap_err());
    assert!(
        err.contains("may not write"),
        "a refusal, not a version: {err}"
    );
    assert!(
        !err.contains("version"),
        "and nothing about the row's version: {err}"
    );
}

#[test]
fn a_stranger_is_not_told_that_a_row_is_private() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "stets-private-2", "stet", "pen:stet");

    let patch = store::Patch {
        key: Some("stets-private-2".into()),
        audience: Some("public".into()),
        asserted_by: Some("cline".into()),
        acted_by: Some("cline".into()),
        ..Default::default()
    };
    let err = format!("{}", store::merge(&conn, &patch).unwrap_err());
    assert!(err.contains("may not write"), "a refusal: {err}");
    assert!(
        !err.contains("private to another pen"),
        "and no confirmation that it is private: {err}"
    );
}

#[test]
fn a_hand_may_reclassify_its_own_row_while_crediting_its_finger() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    store::mint(&conn, "stet", "stet/hand", "self").expect("mint");
    store::mint(&conn, "stet/hand", "stet/hand/finger-1", "finger").expect("mint");
    store::sudo::lend(&conn, "stet/hand", "stet/hand/finger-1", 60).expect("lend");

    row_with_audience(&conn, &embedder, "hands-note", "stet/hand", "public");

    let patch = store::Patch {
        key: Some("hands-note".into()),
        audience: Some("pen:stet,pen:stet/hand".into()),
        asserted_by: Some("stet/hand/finger-1".into()),
        acted_by: Some("stet/hand".into()),
        ..Default::default()
    };
    store::merge(&conn, &patch).expect("the hand answers for its own row");
    let mem = store::get(&conn, "hands-note").unwrap().unwrap();
    assert_eq!(mem.audience, "pen:stet,pen:stet/hand");
    assert_eq!(
        mem.edited_by.as_deref(),
        Some("stet/hand"),
        "the writer, not the credit"
    );
}

#[test]
fn a_row_addressed_to_others_only_is_refused_at_write() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);

    let trap = Memory {
        key: "trap-door".into(),
        title: "for james only".into(),
        body: "and not for me".into(),
        asserted_by: Some("cline".into()),
        audience: "pen:james".into(),
        ..Default::default()
    };
    let err = common::add_now(&conn, &embedder, &trap).expect_err("the trap is refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("invisible to its own author") && msg.contains("cline"),
        "the refusal names the author and the reason: {msg}"
    );
    assert!(
        store::get(&conn, "trap-door").unwrap().is_none(),
        "a refusal writes nothing"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM memory_version"),
        0,
        "no version from a refused write"
    );
    assert_eq!(
        ellmephant::store::encode::pending(&conn).unwrap(),
        0,
        "nothing queued for embedding by a refused write"
    );

    let ok = Memory {
        audience: "pen:james,pen:cline".into(),
        ..trap
    };
    common::add_now(&conn, &embedder, &ok).expect("addressing yourself too is allowed");
    assert!(store::get(&conn, "trap-door").unwrap().is_some());
}

#[test]
fn a_protected_row_asks_the_pen_that_acts_for_the_lease() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    store::mint(&conn, "stet", "stet/hand", "self").expect("mint");
    store::mint(&conn, "stet/hand", "stet/hand/finger-1", "finger").expect("mint");
    store::sudo::lend(&conn, "stet/hand", "stet/hand/finger-1", 60).expect("lend");

    row_with_audience(&conn, &embedder, "protected-note", "stet/hand", "public");
    store::claim::acquire(&conn, "protected-note", "stet/hand", Some(60)).expect("lease");

    let patch = store::Patch {
        key: Some("protected-note".into()),
        tags: Some(vec!["protected".into()]),
        asserted_by: Some("stet/hand/finger-1".into()),
        acted_by: Some("stet/hand".into()),
        ..Default::default()
    };
    store::merge(&conn, &patch).expect("the holder may write under its own lease");
}

#[test]
fn a_live_lease_held_by_someone_else_is_not_permission_to_delete() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "stets-protected", "stet", "public");
    let patch = store::Patch {
        key: Some("stets-protected".into()),
        tags: Some(vec!["protected".into()]),
        asserted_by: Some("stet".into()),
        acted_by: Some("stet".into()),
        ..Default::default()
    };
    store::claim::acquire(&conn, "stets-protected", "stet", Some(60)).expect("lease");
    store::merge(&conn, &patch).expect("stet protects and leases its own row");

    let err = format!(
        "{}",
        store::delete(&conn, "stets-protected", Some("james")).unwrap_err()
    );
    assert!(err.contains("does not hold it"), "{err}");
    assert!(
        store::get(&conn, "stets-protected").unwrap().is_some(),
        "and the row is still there"
    );

    store::delete(&conn, "stets-protected", Some("stet")).expect("the holder may delete");
    assert!(store::get(&conn, "stets-protected").unwrap().is_none());
}

#[test]
fn a_public_row_cannot_be_narrowed_into_the_trap_in_two_steps() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "two-step", "cline", "public");

    let patch = store::Patch {
        key: Some("two-step".into()),
        audience: Some("pen:james".into()),
        asserted_by: Some("cline".into()),
        acted_by: Some("cline".into()),
        ..Default::default()
    };
    let err = format!("{}", store::merge(&conn, &patch).unwrap_err());
    assert!(
        err.contains("invisible to its own author"),
        "narrowing into the trap is refused: {err}"
    );
    assert!(
        err.contains("as:"),
        "and the refusal names the legitimate route for a note to someone else: {err}"
    );
    let mem = store::get(&conn, "two-step").unwrap().unwrap();
    assert_eq!(mem.audience, "public", "nothing changed");
}

#[test]
fn a_retired_authors_row_is_still_writable() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    store::mint(&conn, "james", "ghost", "self").expect("mint a pen that will depart");
    row_with_audience(&conn, &embedder, "ghosts-note", "ghost", "pen:ghost");
    store::retire(
        &conn,
        "ghost",
        "james",
        store::Rows::Inherit,
        store::History::Memorialize,
    )
    .expect("retire it");

    let patch = store::Patch {
        key: Some("ghosts-note".into()),
        tags: Some(vec!["after-death".into()]),
        asserted_by: Some("james".into()),
        acted_by: Some("james".into()),
        ..Default::default()
    };
    store::merge(&conn, &patch).expect("the parent may still tidy what the pen left behind");
}

#[test]
fn an_update_on_an_owned_row_must_name_the_pen_that_acts() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "owned-note", "james", "public");

    let patch = store::Patch {
        key: Some("owned-note".into()),
        body: Some("no actor named".into()),
        asserted_by: Some("cline".into()),
        acted_by: None,
        ..Default::default()
    };
    let err = format!("{}", store::merge(&conn, &patch).unwrap_err());
    assert!(
        err.contains("must name the pen"),
        "the absence is an error, not a fallback: {err}"
    );
    let mem = store::get(&conn, "owned-note").unwrap().unwrap();
    assert_eq!(mem.body, "the facts", "and nothing was written");
}

#[test]
fn a_lend_grants_the_credit_and_not_the_owners_blind_write() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    store::mint(&conn, "stet", "stet/hand", "self").expect("mint");
    store::mint(&conn, "stet/hand", "stet/hand/finger-1", "finger").expect("mint");
    store::sudo::lend(&conn, "stet/hand", "stet/hand/finger-1", 60).expect("lend");

    row_with_audience(
        &conn,
        &embedder,
        "fingers-private",
        "stet/hand/finger-1",
        "public",
    );
    conn.execute(
        "UPDATE memory SET audience = 'pen:stet' WHERE key = 'fingers-private'",
        [],
    )
    .expect("re-address it");
    conn.execute(
        "UPDATE memory SET owner = (SELECT id FROM pen WHERE name = 'stet/hand/finger-1') WHERE key = 'fingers-private'",
        [],
    )
    .expect("own it as the finger");

    let patch = store::Patch {
        key: Some("fingers-private".into()),
        body: Some("the hand writing as its finger".into()),
        asserted_by: Some("stet/hand/finger-1".into()),
        acted_by: Some("stet/hand".into()),
        ..Default::default()
    };
    let err = format!("{}", store::merge(&conn, &patch).unwrap_err());
    assert!(
        err.contains("audience does not include you"),
        "the actor must be able to read what it writes, however the credit is spelled: {err}"
    );
}

#[test]
fn whois_is_invariant_under_the_case_of_the_name() {
    let (_dir, conn) = subtree();
    let base = store::whois(&conn, "cline", "james").expect("the pen exists");
    assert_eq!(base.name, "james", "the row's own spelling");
    for spelling in ["James", "JAMES", "jAmEs", "james"] {
        let w = store::whois(&conn, "cline", spelling).expect("case-insensitive lookup");
        assert_eq!(
            w.name, "james",
            "{spelling} must report the STORED spelling, not echo the query"
        );
        assert_eq!(
            w.relation, base.relation,
            "kin must not depend on how the caller typed it ({spelling})"
        );
    }
    let me = store::whois(&conn, "James", "james").expect("self across cases");
    assert_eq!(me.relation.as_deref(), Some("ME"));
}

#[test]
fn kin_is_invariant_under_the_case_of_the_viewer_too() {
    let (_dir, conn) = subtree();
    let base = store::whois(&conn, "cline", "james").expect("the pen exists");
    for viewer in ["cline", "CLINE", "Cline", "cLiNe"] {
        let w = store::whois(&conn, viewer, "james").expect("viewer resolves case-insensitively");
        assert_eq!(
            w.relation, base.relation,
            "the answer must not depend on how the VIEWER spells itself ({viewer})"
        );
    }
}

#[test]
fn a_legacy_row_with_no_author_can_still_be_written_but_a_new_one_cannot_claim_to_be_it() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);

    row_with_audience(&conn, &embedder, "legacy-orphan", "cline", "public");
    conn.execute(
        "UPDATE memory SET asserted_by = 'anonymous', audience = 'pen:james' WHERE key = 'legacy-orphan'",
        [],
    )
    .expect("make it look like a backfilled row");

    let patch = store::Patch {
        key: Some("legacy-orphan".into()),
        acted_by: Some("cline".into()),
        tags: Some(vec!["tidied".into()]),
        ..Default::default()
    };
    store::merge(&conn, &patch).expect("a legacy unattributed row is not trapped");

    let mem = Memory {
        key: "fresh-anonymous".into(),
        title: "for james only".into(),
        body: "claimed to be nobody".into(),
        asserted_by: Some("anonymous".into()),
        audience: "pen:james".into(),
        ..Default::default()
    };
    let err = format!("{}", common::add_now(&conn, &embedder, &mem).unwrap_err());
    assert!(err.contains("unattributed"), "{err}");
}

#[test]
fn nobody_can_claim_the_anonymous_exemption_to_address_a_row_to_others_only() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    let mem = Memory {
        key: "anonymous-trap".into(),
        title: "for james only".into(),
        body: "and nobody can read it".into(),
        asserted_by: Some("anonymous".into()),
        audience: "pen:james".into(),
        ..Default::default()
    };
    let err = format!("{}", common::add_now(&conn, &embedder, &mem).unwrap_err());
    assert!(
        err.contains("unattributed"),
        "a NEW row may not buy the exemption by naming itself anonymous: {err}"
    );
}

#[test]
fn private_names_the_pen_that_acts_not_the_pen_it_credits() {
    let (_dir, conn) = subtree();
    let _embedder = HashEmbedder::new(DIM);
    store::mint(&conn, "stet", "stet/hand", "self").expect("mint");
    store::mint(&conn, "stet/hand", "stet/hand/finger-1", "finger").expect("mint");
    store::sudo::lend(&conn, "stet/hand", "stet/hand/finger-1", 60).expect("lend");

    let patch = store::Patch {
        key: Some("handed-note".into()),
        title: Some("handed note".into()),
        body: Some("the hand's own words".into()),
        private: Some(true),
        asserted_by: Some("stet/hand/finger-1".into()),
        acted_by: Some("stet/hand".into()),
        ..Default::default()
    };
    let err = format!("{}", store::merge(&conn, &patch).unwrap_err());
    assert!(
        err.contains("contradict") || err.contains("addresses the row to the pen that ACTS"),
        "private and a credit are refused, not silently resolved: {err}"
    );

    let alone = store::Patch {
        key: Some("handed-note".into()),
        title: Some("handed note".into()),
        body: Some("the hand's own words".into()),
        private: Some(true),
        asserted_by: Some("stet/hand".into()),
        acted_by: Some("stet/hand".into()),
        ..Default::default()
    };
    store::merge(&conn, &alone).expect("private alone");
    let mem = store::get(&conn, "handed-note").unwrap().unwrap();
    assert_eq!(
        mem.audience, "pen:stet/hand",
        "private names the actor: {mem:?}"
    );
}

#[test]
fn a_write_to_an_already_trapped_row_says_it_was_already_trapped() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "old-trap", "cline", "public");
    conn.execute(
        "UPDATE memory SET audience = 'pen:james' WHERE key = 'old-trap'",
        [],
    )
    .expect("make the legacy trap");

    let patch = store::Patch {
        key: Some("old-trap".into()),
        acted_by: Some("cline".into()),
        tags: Some(vec!["tidied".into()]),
        ..Default::default()
    };
    let err = format!("{}", store::merge(&conn, &patch).unwrap_err());
    assert!(
        err.contains("IS ALREADY"),
        "the row was already trapped and the message says so: {err}"
    );
    assert!(
        err.contains("did not create that state"),
        "and it says the write did not cause it: {err}"
    );
}

#[test]
fn a_write_that_changes_a_trapped_rows_audience_says_the_new_one_still_excludes_the_author() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "moving-trap", "cline", "public");
    conn.execute(
        "UPDATE memory SET audience = 'pen:james' WHERE key = 'moving-trap'",
        [],
    )
    .expect("legacy trap");

    let patch = store::Patch {
        key: Some("moving-trap".into()),
        acted_by: Some("cline".into()),
        audience: Some("pen:stet".into()),
        ..Default::default()
    };
    let err = format!("{}", store::merge(&conn, &patch).unwrap_err());
    assert!(err.contains("IS ALREADY"), "{err}");
    assert!(
        err.contains("changes the audience"),
        "and it says the audience moved, rather than claiming it did not: {err}"
    );
    assert!(
        !err.contains("does not change the audience"),
        "a false claim about the write, in the message that exists to stop false claims: {err}"
    );
}

#[test]
fn a_legacy_anonymous_row_can_be_made_private_by_the_pen_that_answers_for_it() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "orphan", "cline", "public");
    conn.execute(
        "UPDATE memory SET asserted_by = 'anonymous' WHERE key = 'orphan'",
        [],
    )
    .expect("make it like a backfilled row");

    let patch = store::Patch {
        key: Some("orphan".into()),
        acted_by: Some("cline".into()),
        private: Some(true),
        ..Default::default()
    };
    store::merge(&conn, &patch).expect("an unattributed row can still be made private");
    let mem = store::get(&conn, "orphan").unwrap().unwrap();
    assert_eq!(
        mem.audience, "pen:cline",
        "private names the pen that acts, even when the author is nobody: {mem:?}"
    );
}

#[test]
fn a_row_addressed_to_others_must_name_its_author() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    let mem = Memory {
        key: "nameless".into(),
        title: "addressed to someone".into(),
        body: "with no author".into(),
        asserted_by: None,
        audience: "pen:james".into(),
        ..Default::default()
    };
    let err = format!("{}", common::add_now(&conn, &embedder, &mem).unwrap_err());
    assert!(
        err.contains("names no author"),
        "refused for the right reason: {err}"
    );
    assert!(
        store::get(&conn, "nameless").unwrap().is_none(),
        "and nothing was written"
    );
}

#[test]
fn a_reordered_audience_is_not_a_changed_audience() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "reorder-trap", "cline", "public");
    conn.execute(
        "UPDATE memory SET audience = 'pen:james,pen:stet' WHERE key = 'reorder-trap'",
        [],
    )
    .expect("legacy trap, two addressees");

    let patch = store::Patch {
        key: Some("reorder-trap".into()),
        acted_by: Some("cline".into()),
        audience: Some("pen:stet,pen:james".into()),
        ..Default::default()
    };
    let err = format!("{}", store::merge(&conn, &patch).unwrap_err());
    assert!(err.contains("IS ALREADY"), "{err}");
    assert!(
        !err.contains("changes the audience"),
        "a reorder changed nothing, and the sentence must not say it did: {err}"
    );
}

#[test]
fn private_false_with_an_explicit_audience_is_not_a_contradiction() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row_with_audience(&conn, &embedder, "published-note", "cline", "public");

    let patch = store::Patch {
        key: Some("published-note".into()),
        private: Some(false),
        audience: Some("pen:james,pen:cline".into()),
        acted_by: Some("cline".into()),
        ..Default::default()
    };
    store::merge(&conn, &patch).expect("private: false plus an audience is consistent");
    let after = store::get(&conn, "published-note").unwrap().unwrap();
    assert_eq!(
        after.audience, "pen:james,pen:cline",
        "the audience is the boundary it names"
    );
}
