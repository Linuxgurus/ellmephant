//! Integration: partial writes (`store::merge`) against a real store file and real `vec0` tables.
//!
//! Runs offline against the deterministic [`HashEmbedder`]. What is worth proving here is the
//! per-field law — which writes move the vector, which move the split-review verdict, and which
//! move neither — plus the one thing a field write must never do: change a key.

mod common;

use common::{count, temp_store};
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::index::vec;
use ellmephant::store::{self, Memory, links};
use rusqlite::Connection;

const DIM: usize = 8;

/// Add one memory and hand back `(id, key)`.
fn seed(conn: &Connection, embedder: &HashEmbedder, key: &str, body: &str) -> (i64, String) {
    let mem = Memory {
        key: key.to_string(),
        title: key.to_string(),
        body: body.to_string(),
        tags: vec!["alpha".to_string()],
        ..Default::default()
    };
    common::add_now(conn, embedder, &mem).expect("add memory")
}

/// A patch that names `key` and touches nothing else.
fn bare(key: &str) -> store::Patch {
    store::Patch {
        key: Some(key.to_string()),
        ..Default::default()
    }
}

#[test]
fn a_source_write_keeps_everything_and_touches_no_vector() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let (id, key) = seed(&conn, &embedder, "probe", "the facts");

    let before = store::get(&conn, &key).unwrap().unwrap();
    let vector_before = vec::get(&conn, id).unwrap();

    let out = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            source: Some("cmd: prove /repo".into()),
            ..bare(&key)
        },
    )
    .expect("merge source");

    assert!(!out.queued, "provenance is not part of the document");
    let after = store::get(&conn, &key).unwrap().unwrap();
    assert_eq!(after.source.as_deref(), Some("cmd: prove /repo"));
    assert_eq!(
        (after.title.as_str(), after.body.as_str()),
        (before.title.as_str(), before.body.as_str()),
        "the document did not move"
    );
    assert_eq!(
        vec::get(&conn, id).unwrap(),
        vector_before,
        "vector untouched"
    );
    assert_eq!(
        after.tags, before.tags,
        "an omitted field keeps its stored value"
    );
}

#[test]
fn a_title_write_re_embeds_the_document() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let (_id, key) = seed(&conn, &embedder, "probe", "the facts");

    let out = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            title: Some("a sharper line".into()),
            ..bare(&key)
        },
    )
    .expect("merge title");

    assert!(out.queued, "the title is in the document");
    let after = store::get(&conn, &key).unwrap().unwrap();
    assert_eq!(after.title, "a sharper line");
}

#[test]
fn a_tags_write_re_embeds_nothing() {
    // Tags are FILING, not MEANING — and not part of the document either. So a tag write moves
    // neither the vector nor the document: retro-tagging a memory is cheap and the
    // vector keeps describing what the memory *means*.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let (id, key) = seed(&conn, &embedder, "probe", "the facts");
    let before_row = store::get(&conn, &key).unwrap().unwrap();

    let out = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            tags: Some(vec!["prj:seaweedfs".into(), "Storage".into()]),
            ..bare(&key)
        },
    )
    .expect("merge tags");

    assert!(!out.queued, "tags are not in the document");
    let after = store::get(&conn, &key).unwrap().unwrap();
    assert_eq!(
        (after.title.as_str(), after.body.as_str()),
        (before_row.title.as_str(), before_row.body.as_str()),
        "the document did not move"
    );
    assert_eq!(
        after.tags,
        vec!["prj:seaweedfs".to_string(), "storage".to_string()],
        "membership, normalised and sorted like any tag write"
    );
    assert!(vec::get(&conn, id).unwrap().is_some());
}

#[test]
fn empty_clears_where_it_is_a_fact_and_is_refused_where_it_is_not() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let (_id, key) = seed(&conn, &embedder, "probe", "the facts");
    common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            source: Some("cmd".into()),
            ..bare(&key)
        },
    )
    .expect("set source");

    // Empty IS an instruction where empty is a fact.
    common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            source: Some(String::new()),
            ..bare(&key)
        },
    )
    .expect("clear source");
    assert_eq!(store::get(&conn, &key).unwrap().unwrap().source, None);

    common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            tags: Some(vec![]),
            ..bare(&key)
        },
    )
    .expect("clear tags");
    assert!(store::get(&conn, &key).unwrap().unwrap().tags.is_empty());

    // Empty is a REFUSAL where empty is nonsense — and the refusal writes nothing.
    for (field, patch) in [
        (
            "title",
            store::Patch {
                title: Some(String::new()),
                ..bare(&key)
            },
        ),
        (
            "body",
            store::Patch {
                body: Some(String::new()),
                ..bare(&key)
            },
        ),
    ] {
        let err = common::merge_now(&conn, &embedder, &patch).unwrap_err();
        assert!(
            format!("{err}").contains("cannot be empty"),
            "{field}: {err}"
        );
    }
    let after = store::get(&conn, &key).unwrap().unwrap();
    assert_eq!(after.title, "probe");
    assert_eq!(after.body, "the facts");
}

#[test]
fn a_body_write_moves_the_link_graph() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let (_id, key) = seed(&conn, &embedder, "probe", "jane facts");
    seed(&conn, &embedder, "target", "a target");

    common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            body: Some("now points at [[target]]".into()),
            ..bare(&key)
        },
    )
    .expect("merge body");

    let back = links::backlinks(&conn, "target").expect("backlinks");
    assert!(
        back.iter().any(|b| b.key == key),
        "the link graph moves with the body: {back:?}"
    );
}

#[test]
fn a_new_key_needs_a_title_and_a_body() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    let err = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            key: Some("ghost".into()),
            source: Some("x".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        format!("{err}").contains("needs both a `title` and a `body`"),
        "{err}"
    );
    assert!(
        store::get(&conn, "ghost").unwrap().is_none(),
        "a refused create writes nothing"
    );

    let out = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            key: Some("ghost".into()),
            title: Some("Ghost".into()),
            body: Some("boo".into()),
            ..Default::default()
        },
    )
    .expect("create");
    assert!(out.created, "and it says so");
    assert_eq!(out.key, "ghost");
}

#[test]
fn key_is_an_address_not_a_field() {
    // The one thing a field write must never do. No statement anywhere puts `key` in an UPDATE's
    // SET clause — a key is only ever a predicate (`WHERE key = ?1`) or an INSERT column for a
    // brand-new row — so a rename is not expressible through a patch.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let (id, key) = seed(&conn, &embedder, "anchor", "the facts");

    // Rewrite every writable field at once: the key must not move with them.
    common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            key: Some(key.clone()),
            title: Some("a new title".into()),
            body: Some("new facts".into()),
            source: Some("elsewhere".into()),
            asserted_by: Some("James".into()),
            origin: Some("https://example.com/record".into()),
            tags: Some(vec!["moved".into()]),
            private: None,
            version_note: None,
            expected_version: None,
        },
    )
    .expect("merge everything");

    // PROVENANCE is part of the row now: who asserted this version, and where it came from, kept as
    // separate facts — a relayer names the truth's home without becoming the truth.
    let (asserted, origin): (String, Option<String>) = conn
        .query_row(
            "SELECT asserted_by, origin FROM memory WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("provenance stored");
    assert_eq!(asserted, "James", "a write is attributed to its writer");
    assert_eq!(
        origin.as_deref(),
        Some("https://example.com/record"),
        "origin is the caller's declaration, not the writer"
    );

    let stored: String = conn
        .query_row("SELECT key FROM memory WHERE id = ?1", [id], |r| r.get(0))
        .expect("the row still exists");
    assert_eq!(stored, "anchor", "the key survived a whole-row rewrite");

    // Naming a DIFFERENT key does not rename: it addresses another row, so it creates one.
    let out = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            key: Some("other".into()),
            title: Some("a new title".into()),
            body: Some("new facts".into()),
            ..Default::default()
        },
    )
    .expect("merge other");
    assert!(out.created, "a changed key is a create, never a rename");
    assert!(
        store::get(&conn, "anchor").unwrap().is_some(),
        "the original is untouched"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM memory"), 2);
}

#[test]
fn a_patch_that_changes_nothing_re_embeds_nothing() {
    // Re-writing the same text has to be a no-op, so a retry — or any client that echoes every
    // field back — cannot silently move the document.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let (id, key) = seed(&conn, &embedder, "probe", "the facts");
    let before = store::get(&conn, &key).unwrap().unwrap();
    let vector_before = vec::get(&conn, id).unwrap();

    let out = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            title: Some(before.title.clone()),
            body: Some(before.body.clone()),
            tags: Some(before.tags.clone()),
            ..bare(&key)
        },
    )
    .expect("redundant merge");

    assert!(!out.queued, "the document did not move");
    let after = store::get(&conn, &key).unwrap().unwrap();
    assert_eq!(
        (after.title.as_str(), after.body.as_str()),
        (before.title.as_str(), before.body.as_str()),
        "the document did not move"
    );
    assert_eq!(vec::get(&conn, id).unwrap(), vector_before);
}
