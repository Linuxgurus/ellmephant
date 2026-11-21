mod common;

use common::{count, temp_store};
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::index::vec;
use ellmephant::store::{self, Memory, links};
use rusqlite::Connection;

const DIM: usize = 8;

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
            acted_by: None,
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
            acted_by: None,
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
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let (id, key) = seed(&conn, &embedder, "probe", "the facts");
    let before_row = store::get(&conn, &key).unwrap().unwrap();

    let out = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            acted_by: None,
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
            acted_by: None,
            source: Some("cmd".into()),
            ..bare(&key)
        },
    )
    .expect("set source");

    common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            acted_by: None,
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
            acted_by: None,
            tags: Some(vec![]),
            ..bare(&key)
        },
    )
    .expect("clear tags");
    assert!(store::get(&conn, &key).unwrap().unwrap().tags.is_empty());

    for (field, patch) in [
        (
            "title",
            store::Patch {
                acted_by: None,
                title: Some(String::new()),
                ..bare(&key)
            },
        ),
        (
            "body",
            store::Patch {
                acted_by: None,
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
            acted_by: None,
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
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let (id, key) = seed(&conn, &embedder, "anchor", "the facts");

    common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            acted_by: Some("James".into()),
            key: Some(key.clone()),
            title: Some("a new title".into()),
            body: Some("new facts".into()),
            source: Some("elsewhere".into()),
            asserted_by: Some("James".into()),
            origin: Some("https://example.com/record".into()),
            tags: Some(vec!["moved".into()]),
            private: None,
            audience: None,
            version_note: None,
            expected_version: None,
            force: false,
        },
    )
    .expect("merge everything");

    let (asserted, last_written, origin): (String, String, Option<String>) = conn
        .query_row(
            "SELECT asserted_by, edited_by, origin FROM memory WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("provenance stored");
    assert_eq!(
        asserted, "anonymous",
        "authorship is write-once: the writer of a patch is not the row's author"
    );
    assert_eq!(
        last_written, "James",
        "and the write names itself as the last writer"
    );
    assert_eq!(
        origin.as_deref(),
        Some("https://example.com/record"),
        "origin is the caller's declaration, not the writer"
    );

    let stored: String = conn
        .query_row("SELECT key FROM memory WHERE id = ?1", [id], |r| r.get(0))
        .expect("the row still exists");
    assert_eq!(stored, "anchor", "the key survived a whole-row rewrite");

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
            acted_by: None,
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

#[test]
fn private_and_an_explicit_audience_are_refused_together() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let (_id, key) = seed(&conn, &embedder, "ambiguous", "the facts");

    let err = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            private: Some(true),
            audience: Some("pen:james".into()),
            asserted_by: Some("cline".into()),
            ..bare(&key)
        },
    )
    .expect_err("two contradictory instructions are refused");

    let msg = err.to_string();
    assert!(
        msg.contains("private") && msg.contains("audience"),
        "the refusal names both instructions: {msg}"
    );
    let after = store::get(&conn, &key).unwrap().unwrap();
    assert_eq!(
        after.audience, "public",
        "a refused write changes nothing, in particular not the audience"
    );
}

#[test]
fn a_run_shape_is_refused_and_force_writes_it() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("store");
    let embedder = HashEmbedder::new(DIM);
    let receipt = "EXIT=0 at 14:41Z\nchecked 5 repos\n14:42Z no change\n14:43Z no change";

    let refused = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            acted_by: Some("stet".into()),
            asserted_by: Some("stet".into()),
            key: Some("pr-watch-run-2026-11-08".into()),
            title: Some("a run".into()),
            body: Some(receipt.into()),
            ..Default::default()
        },
    );
    let err = format!("{}", refused.unwrap_err());
    assert!(err.contains("RUN RECORD"), "{err}");
    assert!(
        err.contains("record append source=pr-watch"),
        "the fix is a paste: {err}"
    );
    assert!(err.contains("force: true"), "the override is named: {err}");

    let forced = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            acted_by: Some("stet".into()),
            asserted_by: Some("stet".into()),
            key: Some("pr-watch-run-2026-11-08".into()),
            title: Some("a run".into()),
            body: Some(receipt.into()),
            force: true,
            ..Default::default()
        },
    );
    assert!(forced.is_ok(), "force must override: {:?}", forced.err());

    let lesson = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            acted_by: Some("stet".into()),
            asserted_by: Some("stet".into()),
            key: Some("a-lesson".into()),
            title: Some("a lesson".into()),
            body: Some("★ A TOOL THAT REPORTS SUCCESS MUST BE CHECKED (2026-10-24).".into()),
            ..Default::default()
        },
    );
    assert!(lesson.is_ok(), "one date is a citation: {:?}", lesson.err());
}

/// #257. A banner is an annotated wrong answer: search quotes an excerpt, so the one line that
/// says "this is dead" is the one line no reader ever sees.
#[test]
fn a_banner_is_refused_with_the_replacement_process_named() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("store");
    let embedder = HashEmbedder::new(DIM);
    let bannered = "SUPERSEDED by the 2026-11 checklist.\n\nStep 1: run the old script.\nStep 2: \
wait for the green light.";

    let refused = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            acted_by: Some("stet".into()),
            asserted_by: Some("stet".into()),
            key: Some("old-deploy-checklist".into()),
            title: Some("the old checklist".into()),
            body: Some(bannered.into()),
            ..Default::default()
        },
    );
    let err = format!("{}", refused.unwrap_err());
    assert!(err.contains("BANNER"), "the class is named: {err}");
    assert!(err.contains("SUPERSEDED"), "the word that fired: {err}");
    assert!(
        err.contains("memory_delete") && err.contains("versions_remaining"),
        "the exit deletes, and says the words survive: {err}"
    );
    assert!(
        err.contains("NOT as a `[[wikilink]]`"),
        "the exit must not trade one rot for a dangling edge: {err}"
    );
    assert!(err.contains("force: true"), "the override is named: {err}");
    assert!(
        store::get(&conn, "old-deploy-checklist").unwrap().is_none(),
        "a refused write creates nothing"
    );

    let forced = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            acted_by: Some("stet".into()),
            asserted_by: Some("stet".into()),
            key: Some("old-deploy-checklist".into()),
            title: Some("the old checklist".into()),
            body: Some(bannered.into()),
            force: true,
            ..Default::default()
        },
    );
    assert!(forced.is_ok(), "force must override: {:?}", forced.err());
}

/// The gate is on CREATE. Patching a row never turns it into a banner by accident, and a body
/// that merely DISCUSSES supersession is prose.
#[test]
fn a_banner_gate_does_not_fire_on_patches_or_on_prose() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("store");
    let embedder = HashEmbedder::new(DIM);
    let (_, key) = seed(&conn, &embedder, "deploy-lesson", "the lesson, in one line");

    let patched = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            acted_by: Some("stet".into()),
            asserted_by: Some("stet".into()),
            body: Some(
                "The old checklist was SUPERSEDED in 2026-11; this is what replaced it.".into(),
            ),
            ..bare(&key)
        },
    );
    assert!(
        patched.is_ok(),
        "a patch is not a create: {:?}",
        patched.err()
    );

    let prose = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            acted_by: Some("stet".into()),
            asserted_by: Some("stet".into()),
            key: Some("a-banner-word-mid-body".into()),
            title: Some("a lesson".into()),
            body: Some(
                "A lesson about supersession\n\nAn old row was SUPERSEDED, and the fix was to \
                 delete it rather than annotate it."
                    .into(),
            ),
            ..Default::default()
        },
    );
    assert!(
        prose.is_ok(),
        "only the first line can be a banner: {:?}",
        prose.err()
    );
}
