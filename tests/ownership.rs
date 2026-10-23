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

// ── WRITE REACH ───────────────────────────────────────────────────────────────────────────────────
// Ownership is a rule only if the write path asks. These four tests are the rule: a peer is refused,
// an ancestor is allowed, the commons belongs to everyone, and a finger acts for its parent.

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
    embedder: &HashEmbedder,
    key: &str,
    author: &str,
) -> String {
    let mem = Memory {
        key: key.to_string(),
        title: format!("rewritten by {author}"),
        body: "something else".to_string(),
        asserted_by: Some(author.to_string()),
        ..Default::default()
    };
    match common::add_now(conn, embedder, &mem) {
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

    // The parent's rows; the finger is of the parent, so it reaches them — and still owns nothing.
    let err = rewrite(&conn, &embedder, "shared-work", "stet/finger-7");
    assert!(err.is_empty(), "a finger acts as its parent: {err}");
    assert_eq!(owner(&conn, "shared-work").as_deref(), Some("stet"));
}

// ── REMOVAL IS AN ACT WITH A NAME ────────────────────────────────────────────────────────────────
// A delete is a write of absence: it takes the same reach as an edit, and it leaves a record. A
// removal nobody is named for is indistinguishable from a mistake.

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

// ── MINT AND DISPOSE ─────────────────────────────────────────────────────────────────────────────
// A pen creates a pen, and retires it when the work is done. The parent is the CALLER, never an
// argument: you may create downward, never sideways or upward.

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

    // And a hand mints a finger under itself.
    store::mint(&conn, "stet/hand", "stet/hand/finger-9", "finger").expect("mint a finger");
}

#[test]
fn a_kind_is_never_inferred_from_the_name() {
    let (_dir, conn) = subtree();
    // A slash does not make a finger, and a finger does not need one.
    let minted = store::mint(&conn, "stet", "assistant", "finger").expect("mint");
    assert_eq!(minted.kind, "finger");
    let err = store::mint(&conn, "stet", "weird-kind", "hand").unwrap_err();
    assert!(format!("{err}").contains("`self` or `finger`"), "{err}");
}

#[test]
fn a_name_is_spent_and_a_retired_pen_cannot_mint() {
    let (_dir, conn) = subtree();
    store::mint(&conn, "stet", "stet/old-hand", "self").expect("mint");

    // Minting again over a live pen is refused…
    let err = store::mint(&conn, "stet", "stet/old-hand", "self").unwrap_err();
    assert!(format!("{err}").contains("already a pen"), "{err}");

    // …and after retirement the name is SPENT, not free: two lives under one name would make every
    // ledger row ambiguous.
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

    // A retired pen cannot create anything either.
    let err = store::mint(&conn, "stet/old-hand", "stet/old-hand/child", "finger").unwrap_err();
    assert!(format!("{err}").contains("has been retired"), "{err}");
}

#[test]
fn a_hand_may_credit_its_finger_and_nothing_else() {
    let (_dir, conn) = subtree();
    store::mint(&conn, "stet", "stet/hand", "self").expect("mint");
    store::mint(&conn, "stet/hand", "stet/hand/finger-1", "finger").expect("mint");

    // Itself, and anything below it.
    store::may_act_as(&conn, "stet", "stet").expect("itself");
    store::may_act_as(&conn, "stet/hand", "stet/hand/finger-1").expect("its own finger");
    store::may_act_as(&conn, "stet", "stet/hand/finger-1").expect("down the whole subtree");

    // Never a peer, never an ancestor, never a name nobody minted.
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

    // The finger holds no credential: the hand proves, the finger is credited, the hand OWNS.
    store::may_act_as(&conn, "stet/hand", "stet/hand/finger-2").expect("act as the finger");
    row(&conn, &embedder, "finger-work", "stet/hand/finger-2");
    assert_eq!(owner(&conn, "finger-work").as_deref(), Some("stet/hand"));

    // Disposing of the pen hands its work back to the hand: that is what `retire` is for.
    store::retire(
        &conn,
        "stet/hand/finger-2",
        "stet/hand",
        store::Rows::Inherit,
        store::History::Memorialize,
    )
    .expect("dispose of the finger");
    assert_eq!(owner(&conn, "finger-work").as_deref(), Some("stet/hand"));
    // ★ AND `inherit` IS A NO-OP HERE, correctly: the finger never owned its work, so there is
    // nothing to hand back and nothing to record. A transfer event would be a story about a move
    // that did not happen.
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

// ── KINSHIP ──────────────────────────────────────────────────────────────────────────────────────
// The walk between two pens: shortest, never doubling back, `S` always written. Letters are the fact,
// the word in quotes is a courtesy.

fn deeper_tree(conn: &rusqlite::Connection) {
    conn.execute_batch(
        // Build this tree from an empty registry (0021 seeds `estate`, so keep it and add the rest).
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
    // A child is below, and a grandchild two below.
    assert_eq!(
        kin("stet/hand/f-1", "stet/hand/f-1/f-2").as_deref(),
        Some("C \"child\"")
    );
    assert_eq!(
        kin("stet/hand", "stet/hand/f-1/f-2").as_deref(),
        Some("CC \"grandchild\"")
    );

    // Across the tree: uncle, nephew, cousin — each the shortest walk through the common ancestor.
    assert_eq!(kin("stet/hand", "cline").as_deref(), Some("PS \"uncle\""));
    assert_eq!(kin("cline", "stet/hand").as_deref(), Some("SC \"nephew\""));
    assert_eq!(
        kin("stet/hand", "island").as_deref(),
        None,
        "no common root"
    );

    // And a sibling is never spelled `PC`: the walk is one token.
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

    // f-1's parent is stet/hand; f-1/f-2 is two levels below stet. `cline` is stet's sibling, and
    // `f-2` vs a child of cline would be cousins — build that and check the letters.
    conn.execute(
        "INSERT INTO pen (name, kind, parent_id) VALUES ('cline/hand', 'self', (SELECT id FROM pen WHERE name='cline'))",
        [],
    )
    .unwrap();

    let kin = |a: &str, b: &str| store::relation::relation(&conn, a, b).unwrap().unwrap();
    // stet/hand vs cline/hand: common ancestor james, one step up each side → P S C.
    assert_eq!(kin("stet/hand", "cline/hand").walk, "PSC");
    assert_eq!(kin("stet/hand", "cline/hand").word, Some("cousin"));
    // Four steps up to `james` and two down to `cline/hand`: three P's, the sibling step, one C.
    // English has "first cousin once removed" for this and we do not print it — the letters are
    // exact and the phrase is approximate.
    let removed = kin("stet/hand/f-1/f-2", "cline/hand");
    assert_eq!(removed.walk, "PPPSC");
    assert_eq!(removed.word, None);
}

#[test]
fn english_stops_where_it_starts_approximating() {
    // A word in quotes is a claim, so it is only made where it is exact.
    assert_eq!(
        store::relation::word_for_public("PPSCC"),
        Some("second cousin")
    );
    assert_eq!(store::relation::word_for_public("PPSCCC"), None);
    assert_eq!(store::relation::word_for_public("PS"), Some("uncle"));
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
    assert_eq!(
        seen.descendants, 2,
        "how far it reaches: `f-1` and `f-1/f-2` hang below `stet/hand`, and that is what ancestor read grants"
    );
    assert!(seen.ceased.is_none());

    // The relation is from the ASKER's seat: the same pen is a CHILD to its parent and a NEPHEW to
    // a sibling of that parent.
    let from_cline = store::whois(&conn, "cline", "stet/hand").expect("whois");
    assert_eq!(from_cline.relation.as_deref(), Some("SC \"nephew\""));
    let from_self = store::whois(&conn, "stet/hand", "stet/hand").expect("whois");
    assert_eq!(from_self.relation.as_deref(), Some("ME"));

    // A pen from another estate is reachable by name and related to nobody here.
    let root = store::whois(&conn, "estate", "estate").expect("whois");
    assert_eq!(root.descendants, 6, "the root reaches everything below it");

    let island = store::whois(&conn, "stet", "island").expect("whois");
    assert_eq!(
        island.relation, None,
        "no common root is an answer, not an error"
    );

    // And a retired pen still answers, with its tombstone.
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

    // A name nobody minted is a refusal that says so.
    let err = store::whois(&conn, "stet", "nobody-at-all").unwrap_err();
    assert!(format!("{err}").contains("no pen named"), "{err}");
}
