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
    // ★ THIS IS THE ONLY WRITE WHERE THE TWO COLUMNS MOVE INDEPENDENTLY, so the split is only
    // exercised if they DISAGREE here (Stet, review): `asserted_by` names the pen that did it,
    // `owner` the pen that answers for it. If they came out equal, nothing was being tested.
    let mem = store::get(&conn, "by-a-finger").unwrap().unwrap();
    assert_eq!(
        mem.asserted_by.as_deref(),
        Some("stet/finger-3"),
        "who did it"
    );
    assert_eq!(mem.owner.as_deref(), Some("stet"), "who answers for it");
    assert_ne!(
        mem.asserted_by, mem.owner,
        "the split, exercised rather than assumed"
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

#[test]
fn stats_separate_the_commons_from_the_unattributed() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);

    // A released row: no owner, but the author is NAMED. That is the commons, and a decision.
    row(&conn, &embedder, "released", "stet");
    store::disclaim(&conn, "released", "stet", false).expect("disclaim");

    // A row with no author at all — what the HTTP door produced for every write until 2026-10-23.
    // Nobody is answerable AND nobody is named: a bug, and it must not be counted as a commons.
    // `asserted_by` is NOT NULL and defaults to `anonymous`, so "nobody is named" is spelled with
    // the reserved word rather than NULL — which is what makes the two cases separable at all.
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
fn a_deletion_of_an_owned_row_records_a_retire_in_effect() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "owned-note", "stet");

    // Stet's rule: "delete for what has no owner, retire for what has one — and if an owned row is
    // deleted it needs a ledger row anyway, at which point it IS a retire". This is that ledger row.
    assert!(store::delete(&conn, "owned-note", Some("stet")).expect("delete"));
    let events = store::pen_events(&conn, "owned-note").unwrap();
    let entry = events
        .iter()
        .find(|e| e.1 == "delete")
        .expect("a ledger row");
    assert_eq!(entry.2.as_deref(), Some("stet"), "who held it");
    assert_eq!(entry.4.as_deref(), Some("stet"), "who ended it");
}

#[test]
fn the_ownerless_rows_can_be_listed_not_only_counted() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);

    // Disclaimed BY CHOICE — what an ancestor does to a retiring descendant's rows when it does not
    // want them. The author stays named; nobody owns it. A supported state, not damage.
    row(&conn, &embedder, "released", "stet");
    store::disclaim(&conn, "released", "stet", false).expect("disclaim");

    // No author at all: a write made with no pen, or a pen that is gone.
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

    // And the listing is the point: a claimer needs the ADDRESS, not the total.
    assert!(
        rows.iter()
            .all(|r| !r.key.is_empty() && !r.created.is_empty())
    );
}

#[test]
fn the_unowned_listing_obeys_the_audience_tier() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);

    // A public ownerless row: visible to anyone, as the commons should be.
    row(&conn, &embedder, "released", "stet");
    store::disclaim(&conn, "released", "stet", false).expect("disclaim");

    // A PRIVATE row that is also ownerless — the case the first version of this listing would have
    // leaked: nobody owns it, but it is still addressed to one pen.
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

// ── ABSENCE, APPLIED TO DESTRUCTION (final adversarial pass) ─────────────────────────────────────
// The private tier's law is "absence, not refusal". Two destructive paths were answering as oracles
// instead: a DELETE said "owned by stet" for a row the caller could not read, and a MERGE deleted the
// source without asking anyone's leave.

#[test]
fn destroying_an_invisible_private_row_answers_as_if_it_never_existed() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-secret", "stet");
    conn.execute(
        "UPDATE memory SET audience = 'pen:stet' WHERE key = 'stets-secret'",
        [],
    )
    .unwrap();

    // cline is a PEER: the row does not exist for it, so the refusal must not confirm it does.
    let err = store::delete(&conn, "stets-secret", Some("cline")).unwrap_err();
    assert!(
        format!("{err}").contains("no such memory"),
        "an invisible row must answer as absent, not report its owner: {err}"
    );
    // …and it is still there.
    assert!(store::get(&conn, "stets-secret").unwrap().is_some());

    // Its owner, and its ancestor, still reach it.
    assert!(store::delete(&conn, "stets-secret", Some("stet")).is_ok());
}

#[test]
fn a_merge_is_a_delete_and_is_gated_like_one() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-note", "stet");
    row(&conn, &embedder, "clines-note", "cline");
    row(&conn, &embedder, "stets-other", "stet");

    // A peer may not destroy stet's row by "merging" it into its own.
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

    // Its owner may — into its OWN row. Merging into a peer's row would rewrite the peer's body, so
    // `into` is gated too, and that refusal is the second half of the same rule.
    let err =
        store::links::merge_into(&conn, "stets-note", "clines-note", Some("stet")).unwrap_err();
    assert!(
        format!("{err}").contains("may not write"),
        "the survivor is somebody else's: {err}"
    );
    assert!(store::links::merge_into(&conn, "stets-note", "stets-other", Some("stet")).is_ok());
}
