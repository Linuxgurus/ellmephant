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

    // ★ AND `publish=true` IS NOT ENOUGH IF YOU CANNOT READ IT (James, 2026-10-24: `pen:` is EXACT).
    // `stet` is the ancestor, but the row is addressed `pen:stet/mid/leaf`, so stet cannot see one
    // word of it. Publishing unread words is a leak with extra steps.
    let err = store::disclaim(&conn, "private-work", "stet", true)
        .expect_err("an ancestor may not publish what it cannot read");
    assert!(format!("{err}").contains("cannot publish"), "{err}");

    // Shared with the ancestor by name, the same act is allowed — and the ledger calls it a publish.
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
fn destroying_an_invisible_private_row_says_forbidden_not_absent() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-secret", "stet");
    conn.execute(
        "UPDATE memory SET audience = 'pen:stet' WHERE key = 'stets-secret'",
        [],
    )
    .unwrap();

    // cline is a PEER. A READ stays absent — the tier's law — but a WRITE says plainly that the
    // answer is no: "no such memory" for a memory that exists sends an agent hunting a bug that is not
    // there (James, 2026-10-24).
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

    // Itself, always: a pen is itself without anyone's leave.
    store::may_act_as(&conn, "stet", "stet").expect("itself");

    // ★ A DESCENDANT IS NOT ENOUGH (James tie-break, 2026-10-23): parentage is necessary, a LIVE
    // LEND is what permits. Without it, a finger's authority would be a fact about tree shape and
    // nothing could revoke it short of re-shaping the tree.
    let err = store::may_act_as(&conn, "stet/hand", "stet/hand/finger-1").unwrap_err();
    assert!(format!("{err}").contains("LIVE lend"), "{err}");

    let lent = store::sudo::lend(&conn, "stet/hand", "stet/hand/finger-1", 60).expect("lend");
    assert_eq!(lent.powerful, "stet/hand");
    assert_eq!(lent.weak, "stet/hand/finger-1");
    store::may_act_as(&conn, "stet/hand", "stet/hand/finger-1")
        .expect("its own finger, while lent");
    // An ancestor lends its own authority to a deeper descendant too: the walk is downward.
    store::sudo::lend(&conn, "stet", "stet/hand/finger-1", 60).expect("lend down the subtree");
    store::may_act_as(&conn, "stet", "stet/hand/finger-1")
        .expect("down the whole subtree, while lent");

    // Revoking is one deleted row, and the very next check refuses — with no cache in between.
    store::sudo::release(&conn, "stet/hand/finger-1", "stet/hand").expect("release");
    let err = store::may_act_as(&conn, "stet/hand", "stet/hand/finger-1").unwrap_err();
    assert!(
        format!("{err}").contains("LIVE lend"),
        "revocation is immediate: {err}"
    );

    // And a lend expires on its own: move the clock in the row and the permission is gone.
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

    // The finger holds no credential: the hand proves, the finger is credited, the hand OWNS — and
    // the right to credit it is a lend that expires, not an inheritance that does not.
    store::sudo::lend(&conn, "stet/hand", "stet/hand/finger-2", 300).expect("lend");
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

#[test]
fn a_pen_cannot_be_named_anonymous_or_given_an_unaddressable_name() {
    let (_dir, conn) = subtree();

    // `anonymous` is the store's word for "no currently existing pen". A pen by that name would make
    // every unattributed count ambiguous — the defect 0021/0025 remove.
    for name in ["anonymous", "ANONYMOUS", " anonymous "] {
        let err = store::mint(&conn, "stet", name, "finger").unwrap_err();
        assert!(format!("{err}").contains("reserved"), "{name}: {err}");
    }

    // A private row's audience is `pen:<name>`; a comma or a space cannot be addressed there, so the
    // pen could never hold one — and nobody would find out until the first private write failed.
    for name in ["stet/finger,1", "stet finger"] {
        let err = store::mint(&conn, "stet", name, "finger").unwrap_err();
        assert!(format!("{err}").contains("addressed"), "{name}: {err}");
    }

    // And a name that can be addressed still works.
    store::mint(&conn, "stet", "stet/finger-9f3a1c", "finger").expect("a good name");
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
    assert!(seen.ceased.is_none());

    // The relation is from the ASKER's seat: the same pen is a CHILD to its parent and a NEPHEW to
    // a sibling of that parent.
    let from_cline = store::whois(&conn, "cline", "stet/hand").expect("whois");
    assert_eq!(from_cline.relation.as_deref(), Some("SC \"nephew\""));
    let from_self = store::whois(&conn, "stet/hand", "stet/hand").expect("whois");
    assert_eq!(from_self.relation.as_deref(), Some("ME"));

    // A pen from another estate is reachable by name and related to nobody here.
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

#[test]
fn an_ancestor_writes_a_descendants_row_and_the_attribution_says_so() {
    let (_dir, conn) = subtree();
    let embedder = HashEmbedder::new(DIM);
    row(&conn, &embedder, "stets-note", "stet");

    // James, 2026-10-24: "an ancestor always has (rarely used) ability to write on descendant-owned
    // data, with proper attribution". No lend is needed for that — the lend is only for CREDITING the
    // write to the descendant. Here the writer is the ancestor, and the record says so.
    let rewrite = Memory {
        key: "stets-note".into(),
        title: "rewritten by james".into(),
        body: "the ancestor's correction".into(),
        asserted_by: Some("james".into()),
        ..Default::default()
    };
    common::add_now(&conn, &embedder, &rewrite).expect("an ancestor may write down its subtree");

    let mem = store::get(&conn, "stets-note").unwrap().unwrap();
    assert_eq!(
        mem.asserted_by.as_deref(),
        Some("james"),
        "the ancestor is credited, not the owner"
    );
    assert_eq!(
        mem.edited_by.as_deref(),
        Some("james"),
        "and the edit is named"
    );
    assert_eq!(
        mem.owner.as_deref(),
        Some("stet"),
        "responsibility did not move"
    );
}

// ── THE TREE, FROM CONFIGURATION (2026-10-24) ────────────────────────────────────────────────────
// The seed gives names; provisioning gives standing — and "provisioning" had no mechanism: no sqlite3
// in the image, no mounting the PVC beside a running brain, no API. A store would roll with five roots
// and every ancestry rule would reach nothing while it reported itself ready.

#[test]
fn the_tree_attaches_from_configuration_and_becomes_one_root() {
    let (_dir, conn) = subtree();
    // subtree() gives estate → james → {stet, cline}; work from a flat registry instead.
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
    // And the rules that were inert are live: estate reaches stet, stet's sibling does not.
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

    // a loop
    store::set_parent(&conn, "james", "estate").unwrap();
    let err = store::set_parent(&conn, "estate", "james").unwrap_err();
    assert!(format!("{err}").contains("loop"), "{err}");
    // itself
    let err = store::set_parent(&conn, "stet", "stet").unwrap_err();
    assert!(format!("{err}").contains("own parent"), "{err}");
    // a parent nobody minted
    let err = store::set_parent(&conn, "stet", "nobody-at-all").unwrap_err();
    assert!(format!("{err}").contains("no pen named"), "{err}");

    // idempotence: repeating an edge is not an error and writes nothing new.
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
    // estate → james → {stet, cline} is the seeded shape; now somebody moves stet ON PURPOSE.
    store::set_parent(&conn, "stet", "estate").unwrap();

    // The next boot re-reads the same config. It must not undo the deliberate act: a restart is exactly
    // when nobody is watching (James, 2026-10-24: "the estate is in an environment variable instead of a
    // table?" — it is not, and it must not be).
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

// ── CASE: INSENSITIVE, PRESERVING (James, 2026-10-24) ────────────────────────────────────────────
// The identity table matches claims with `eq_ignore_ascii_case` and `audience::pen()` lowercases, so every
// layer above treats `James` and `james` as one pen. The table must agree, or the row you authenticated as
// need not be the row that owns your memories.

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
    // `retire_preview` goes through `pen_row`, which is the lookup every verb uses.
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
    // And the triggers hold even without the index: a direct INSERT differing only in case is refused.
    let bad = conn.execute(
        "INSERT INTO pen (name, kind, created) VALUES ('STET', 'self', datetime('now'))",
        [],
    );
    assert!(
        bad.is_err(),
        "a case-colliding INSERT must be refused by the trigger"
    );
}

// ── A HELD SESSION IS AUTHORITY THE CHECKS CAN SEE (2026-10-24) ──────────────────────────────────
// Before this, `active_sudo` was a ledger nothing consulted: a pen could hold the estate for an hour and
// still be refused every row outside its own branch. Found by running the verbs live and watching them
// change nothing.

#[test]
fn holding_a_session_reaches_the_powerful_pens_rows_and_stops_when_released() {
    let (_dir, conn) = subtree();
    // The PRODUCTION shape (James, 2026-10-24): estate → (james → cline, stet → …). `subtree()` still
    // builds the older one; moved here so the test is about the live tree, where stet is cline's UNCLE.
    store::set_parent(&conn, "stet", "estate").unwrap();
    let embedder = HashEmbedder::new(DIM);
    // A private row deep in stet's branch: cline is stet's NEPHEW, so it cannot see it.
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

    // James borrows the estate (a policy is required for sudo).
    store::sudo::grant(&conn, "estate", "james", 60, "test").unwrap();
    store::sudo::sudo(&conn, "james", "estate", 30).unwrap();
    assert!(sees("james"), "holding the estate reaches every branch");

    // Released: the reach stops with the row, not with a cache.
    store::sudo::release(&conn, "james", "estate").unwrap();
    assert!(!sees("james"), "released means released");
}

// ── ADVERSARIAL PASS, 2026-10-24 ─────────────────────────────────────────────────────────────────
// Two defects found by re-reading the night's own work.

#[test]
fn a_lend_dies_when_the_parentage_that_justified_it_moves() {
    let (_dir, conn) = subtree(); // estate → james → {stet, cline}
    let embedder = HashEmbedder::new(DIM);
    registry(&conn, "james-secret-hand", Some("james"));
    // Addressed EXACTLY to james: `pen:` names one pen, so nothing but a lend (or the pen itself)
    // can reach it.
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

    // James lends down to cline — legal, because cline is below him.
    store::sudo::lend(&conn, "james", "cline", 600).unwrap();
    assert!(sees("cline"), "the lend is live");

    // ★ And now the tree moves underneath it: cline is adopted out of james's branch. The session row
    // still exists, unexpired — and it must stop answering, because a lend is parentage PLUS a clock.
    // A session that outlives its justification is a credential.
    store::set_parent(&conn, "cline", "estate").unwrap();
    assert!(
        !sees("cline"),
        "a lend whose parentage has moved grants nothing, even before it expires"
    );
}

#[test]
fn a_borrowed_read_is_recorded_as_borrowed_not_as_a_subtree_read() {
    let (_dir, conn) = subtree();
    // Production shape: stet is james's SIBLING, so only a borrow reaches him.
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
    // A row shared to the reader's own branch is a SUBTREE read, not a borrowed one — cline is below
    // james, so `subtree:james` reaches it with no session at all.
    assert!(
        !reader.is_borrowed("subtree:james"),
        "a share aimed at the reader's own branch is a subtree read and must stay labelled so"
    );
    // The audit's vocabulary accepts the new reason (migration 0028 rebuilt the CHECK constraint).
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

// ── SHARED PRIVATE AUDIENCES (James, 2026-10-24) ────────────────────────────────────────────────
// "I could set all my tax stuff private, visible only to either me or you."
// Grammar: `pen:<name>` is EXACTLY that pen; `subtree:<name>` is that pen and everything below it.
// A pen above you no longer reads your private rows by ancestry — that was the old law and he
// removed it: an ancestor must be named, or use `subtree:`.

fn shared(conn: &rusqlite::Connection, embedder: &HashEmbedder, key: &str, audience: &str) {
    let mem = Memory {
        key: key.into(),
        title: "shared".into(),
        body: "one fact, two readers".into(),
        asserted_by: Some("james".into()),
        audience: audience.into(),
        ..Default::default()
    };
    common::add_now(conn, embedder, &mem).expect("write");
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
    store::set_parent(&conn, "stet", "estate").unwrap(); // production shape
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
    store::set_parent(&conn, "stet", "estate").unwrap(); // production shape: stet is a sibling
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
