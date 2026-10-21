//! The word index, end to end: FTS5 over `memory`, with the trigger discipline that makes it honest
//! (soma/ellmephant#62).
//!
//! The trap this test file exists for, paid for once already in `spine` v7: an external-content FTS5
//! table does **not** maintain itself. A missing UPDATE trigger leaves the index serving text that no
//! longer exists — silently, for weeks — because a stale hit looks exactly like a hit.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::index::fts;
use ellmephant::store::{self, Memory};

fn mem(key: &str, title: &str, body: &str) -> Memory {
    Memory {
        key: key.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

#[test]
fn an_insert_is_searchable_by_its_words() {
    let (_d, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    store::add(
        &conn,
        &mem(
            "velero",
            "Velero backups",
            "velero restores from the bucket",
        ),
    )
    .expect("add");

    let hits = fts::search(&conn, "bucket", 10).expect("search");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(
        hits[0].bm25 < 0.0,
        "BM25 is negative, lower is better: {hits:?}"
    );

    // ★ AN EXACT IDENTIFIER IS FOUND BY ITS TOKENS, not by a model's opinion of meaning. This is the
    // whole point: `pr53-verification` tokenises to pr53 + verification.
    store::add(
        &conn,
        &mem("pr53-verification", "PR 53 review", "checked the migration"),
    )
    .expect("add");
    let hits = fts::search(&conn, "pr53-verification", 10).expect("search");
    let keys = store::all(&conn)
        .expect("all")
        .into_iter()
        .filter(|m| hits.iter().any(|h| m.id == Some(h.id)))
        .map(|m| m.key)
        .collect::<Vec<_>>();
    assert!(keys.contains(&"pr53-verification".to_string()), "{keys:?}");
}

/// ★ THE TRIGGER TEST — the reason this module can be trusted. An edit must be found by its NEW text
/// and must NOT be found by its OLD text; anything else is a stale index.
#[test]
fn an_update_is_found_by_its_new_text_and_not_its_old() {
    let (_d, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    store::add(
        &conn,
        &mem("colours", "Palette", "the archive is coloured blue"),
    )
    .expect("add");
    assert_eq!(fts::search(&conn, "blue", 10).expect("search").len(), 1);

    let patch = store::Patch {
        key: Some("colours".into()),
        body: Some("the archive is coloured green now".into()),
        ..Default::default()
    };
    store::merge(&conn, &patch).expect("merge");

    assert_eq!(
        fts::search(&conn, "green", 10).expect("search").len(),
        1,
        "the NEW text must be indexed"
    );
    assert!(
        fts::search(&conn, "blue", 10).expect("search").is_empty(),
        "the OLD text must be GONE — a missing UPDATE trigger leaves it here forever"
    );
}

/// A delete removes the row from the index. With external content this needs the `'delete'` command
/// carrying the old values; a plain DELETE cannot work.
#[test]
fn a_delete_leaves_nothing_searchable() {
    let (_d, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    store::add(
        &conn,
        &mem("gone", "Gone", "a phrase that will not survive"),
    )
    .expect("add");
    assert_eq!(fts::search(&conn, "survive", 10).expect("search").len(), 1);
    store::delete(&conn, "gone").expect("delete");
    assert!(
        fts::search(&conn, "survive", 10)
            .expect("search")
            .is_empty(),
        "a deleted row must not be searchable by its text"
    );
}

/// A TITLE edit moves the tokens too — the same UPDATE trigger, from another column. Worth its own
/// test because the trigger names all three columns and a typo in one would leave that column stale.
#[test]
fn a_title_edit_is_searchable_by_the_new_title() {
    let (_d, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    store::add(&conn, &mem("t", "Old headline", "body")).expect("add");
    assert_eq!(fts::search(&conn, "headline", 10).expect("search").len(), 1);
    let patch = store::Patch {
        key: Some("t".into()),
        title: Some("Fresh headline".into()),
        ..Default::default()
    };
    store::merge(&conn, &patch).expect("merge");
    assert_eq!(fts::search(&conn, "fresh", 10).expect("search").len(), 1);
}
