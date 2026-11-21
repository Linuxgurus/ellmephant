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
    store::delete(&conn, "gone", None).expect("delete");
    assert!(
        fts::search(&conn, "survive", 10)
            .expect("search")
            .is_empty(),
        "a deleted row must not be searchable by its text"
    );
}

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
