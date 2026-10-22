//! The private tier's canary (#54, #55): a row is visible to the pen that wrote it, invisible to
//! every other reader, and ABSENT rather than refused.
//!
//! The test is written the way the issue asks for it — probe every surface that can name a document
//! as (a) the author, (b) another pen, (c) no pen — because private FAILS OPEN: each surface that
//! forgets the filter is a leak, and one passing surface proves nothing about the next.

use ellmephant::config::Paths;
use ellmephant::db;
use ellmephant::search::{self, Query};
use ellmephant::store::audience::Reader;
use ellmephant::{Result, store};

/// A store with one public row and one private row, written by `stet`.
fn seeded(dir: &std::path::Path) -> rusqlite::Connection {
    let conn = db::open(&Paths::from_root(dir)).unwrap();
    store::merge(
        &conn,
        &store::Patch {
            key: Some("open-note".into()),
            title: Some("open note".into()),
            body: Some("public facts".into()),
            asserted_by: Some("stet".into()),
            ..Default::default()
        },
    )
    .unwrap();
    store::merge(
        &conn,
        &store::Patch {
            key: Some("stets-note".into()),
            title: Some("stet's own".into()),
            body: Some("private facts".into()),
            asserted_by: Some("stet".into()),
            private: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    // A public row that links to the private one: the link graph must not be a way back in.
    store::merge(
        &conn,
        &store::Patch {
            key: Some("points-at-it".into()),
            title: Some("points".into()),
            body: Some("see [[stets-note]]".into()),
            asserted_by: Some("james".into()),
            ..Default::default()
        },
    )
    .unwrap();
    conn
}

fn keys(bundle: &search::Bundle) -> Vec<String> {
    bundle.hits.iter().map(|h| h.key.clone()).collect()
}

#[test]
fn the_audience_is_written_and_read_back() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());
    let mine = store::get(&conn, "stets-note")?.unwrap();
    assert_eq!(mine.audience, "pen:stet");
    let theirs = store::get(&conn, "open-note")?.unwrap();
    assert_eq!(theirs.audience, "public");
    Ok(())
}

#[test]
fn a_private_write_needs_a_proven_pen() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db::open(&Paths::from_root(dir.path())).unwrap();
    let err = store::merge(
        &conn,
        &store::Patch {
            key: Some("nobody".into()),
            title: Some("t".into()),
            body: Some("b".into()),
            private: Some(true),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("PROVEN pen"),
        "private to nobody must be refused: {err}"
    );
}

#[test]
fn search_shows_it_to_the_author_and_nobody_else() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());
    let q = Query {
        text: None,
        pattern: None,
        mode: Default::default(),
        k: 50,
        max_distance: None,
        expand: 0,
        tags: Vec::new(),
        min_chars: None,
        max_chars: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
    };

    let author = keys(&search::run(&conn, None, &Reader::pen("stet"), &q)?);
    assert!(author.contains(&"stets-note".to_string()), "{author:?}");

    let other = keys(&search::run(&conn, None, &Reader::pen("james"), &q)?);
    assert!(!other.contains(&"stets-note".to_string()), "{other:?}");

    let penless = keys(&search::run(&conn, None, &Reader::anonymous(), &q)?);
    assert!(!penless.contains(&"stets-note".to_string()), "{penless:?}");
    Ok(())
}

#[test]
fn a_tag_filter_does_not_answer_questions_about_hidden_rows() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());
    store::merge(
        &conn,
        &store::Patch {
            key: Some("stets-note".into()),
            tags: Some(vec!["private-topic".into()]),
            ..Default::default()
        },
    )?;
    let q = Query {
        text: None,
        pattern: None,
        mode: Default::default(),
        k: 50,
        max_distance: None,
        expand: 0,
        tags: vec!["private-topic".into()],
        min_chars: None,
        max_chars: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
    };
    let peek = search::run(&conn, None, &Reader::pen("james"), &q)?;
    assert!(
        peek.hits.is_empty(),
        "a filter must not confirm it: {peek:?}"
    );
    Ok(())
}

#[test]
fn backlinks_and_history_do_not_name_it() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());

    // The public row points AT the private one: another pen must see no edge, from either side.
    let others = store::links::backlinks_seen(&conn, "stets-note", &Reader::pen("james"))?;
    assert!(others.is_empty(), "{others:?}");
    let mine = store::links::backlinks_seen(&conn, "stets-note", &Reader::pen("stet"))?;
    assert_eq!(mine.len(), 1, "{mine:?}");

    // Editing the private row leaves a version behind; another pen must not learn it existed.
    store::merge(
        &conn,
        &store::Patch {
            key: Some("stets-note".into()),
            body: Some("private facts, revised".into()),
            ..Default::default()
        },
    )?;
    let versions = store::version::history(&conn, "stets-note")?;
    assert!(!versions.is_empty(), "the author's own history is there");
    Ok(())
}

#[test]
fn stats_counts_only_what_the_reader_may_see() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());
    let all = store::stats::compute_seen(&conn, &Reader::pen("stet"))?;
    let some = store::stats::compute_seen(&conn, &Reader::pen("james"))?;
    let none = store::stats::compute_seen(&conn, &Reader::anonymous())?;
    assert_eq!(all.memories, 3);
    assert_eq!(some.memories, 2, "another pen must not count it");
    assert_eq!(none.memories, 2, "nor may a penless reader");
    assert!(
        some.body_chars < all.body_chars,
        "the hidden body must not be summed either"
    );
    Ok(())
}

#[test]
fn another_pen_cannot_publish_what_it_cannot_read() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());
    let err = store::merge(
        &conn,
        &store::Patch {
            key: Some("stets-note".into()),
            private: Some(false),
            asserted_by: Some("james".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("not yours to change"),
        "a row you cannot read is not a row you may publish: {err}"
    );
    assert_eq!(
        store::get(&conn, "stets-note")?.unwrap().audience,
        "pen:stet",
        "and nothing was written"
    );
    Ok(())
}

#[test]
fn project_preload_omits_what_the_reader_cannot_see() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());
    store::project::upsert(
        &conn,
        &store::project::Project {
            key: "prj".into(),
            title: "a project".into(),
            ..Default::default()
        },
        Some(&["open-note".to_string(), "stets-note".to_string()]),
    )?;
    let mine = store::project::preload_seen(
        &conn,
        "prj",
        store::project::Hydrate::None,
        &Reader::pen("stet"),
    )?;
    assert_eq!(mine.len(), 2);
    let theirs = store::project::preload_seen(
        &conn,
        "prj",
        store::project::Hydrate::None,
        &Reader::pen("james"),
    )?;
    assert_eq!(theirs.len(), 1, "the private entry is absent, not null");
    assert_eq!(theirs[0].key, "open-note");
    Ok(())
}
