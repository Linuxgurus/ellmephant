use ellmephant::config::Paths;
use ellmephant::db;
use ellmephant::search::{self, Query};
use ellmephant::store::audience::Reader;
use ellmephant::{Result, store};

fn seeded(dir: &std::path::Path) -> rusqlite::Connection {
    let conn = db::open(&Paths::from_root(dir)).unwrap();
    store::merge(
        &conn,
        &store::Patch {
            key: Some("open-note".into()),
            title: Some("open note".into()),
            body: Some("public facts".into()),
            acted_by: Some("stet".into()),
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
            acted_by: Some("stet".into()),
            asserted_by: Some("stet".into()),
            private: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    store::merge(
        &conn,
        &store::Patch {
            key: Some("points-at-it".into()),
            title: Some("points".into()),
            body: Some("see [[stets-note]]".into()),
            acted_by: Some("james".into()),
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
        semantic_only: false,
        expand: 0,
        tags: Vec::new(),
        min_chars: None,
        max_chars: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
        key: None,
        key_prefix: false,
        orphans: false,
        untagged: false,
        title: None,
        status: None,
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
            acted_by: Some("stet".into()),
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
        semantic_only: false,
        expand: 0,
        tags: vec!["private-topic".into()],
        min_chars: None,
        max_chars: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
        key: None,
        key_prefix: false,
        orphans: false,
        untagged: false,
        title: None,
        status: None,
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

    let others = store::links::backlinks_seen(&conn, "stets-note", &Reader::pen("james"))?;
    assert!(others.is_empty(), "{others:?}");
    let mine = store::links::backlinks_seen(&conn, "stets-note", &Reader::pen("stet"))?;
    assert_eq!(mine.len(), 1, "{mine:?}");

    store::merge(
        &conn,
        &store::Patch {
            key: Some("stets-note".into()),
            acted_by: Some("stet".into()),
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
            acted_by: Some("james".into()),
            asserted_by: Some("james".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("does not include you"),
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

fn tree(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "INSERT OR IGNORE INTO pen (name, kind) VALUES ('estate', 'self');
         INSERT OR IGNORE INTO pen (name, kind) VALUES ('cline', 'self');
         UPDATE pen SET parent_id = (SELECT id FROM pen WHERE name = 'estate') WHERE name = 'james';
         UPDATE pen SET parent_id = (SELECT id FROM pen WHERE name = 'james')  WHERE name = 'stet';
         UPDATE pen SET parent_id = (SELECT id FROM pen WHERE name = 'james')  WHERE name = 'cline';",
    )
    .expect("pen tree");
}

fn all_q() -> Query {
    Query {
        text: None,
        pattern: None,
        mode: Default::default(),
        k: 50,
        max_distance: None,
        semantic_only: false,
        expand: 0,
        tags: Vec::new(),
        min_chars: None,
        max_chars: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
        key: None,
        key_prefix: false,
        orphans: false,
        untagged: false,
        title: None,
        status: None,
    }
}

#[test]
fn an_ancestor_does_not_read_a_descendants_private_row_but_a_share_reaches() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());
    tree(&conn);

    let parent = Reader::pen("james").in_tree(&conn);
    assert!(
        !parent.sees("pen:stet"),
        "an ancestor is not implied any more"
    );
    let hits = keys(&search::run(&conn, None, &parent, &all_q())?);
    assert!(!hits.contains(&"stets-note".to_string()), "{hits:?}");

    let shared = Reader::pen("james").in_tree(&conn);
    assert!(shared.sees("subtree:james"), "a share aimed at my branch");
    assert!(
        !shared.sees("subtree:estate-other"),
        "and only the addresses I actually hold"
    );
    let sibling = Reader::pen("cline").in_tree(&conn);
    assert!(
        !sibling.sees("subtree:stet"),
        "another branch is another branch"
    );

    let stranger = Reader::pen("estate-other").in_tree(&conn);
    assert_eq!(stranger.down(), 0, "a pen with no row reaches nothing");
    assert!(!stranger.sees("pen:stet"));
    Ok(())
}

#[test]
fn the_root_reads_everything_and_the_predicate_agrees_with_sees() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());
    tree(&conn);

    let root = Reader::pen("estate").as_root();
    assert!(root.sees("pen:stet"), "the root reads every address");
    assert!(root.sees("subtree:james,pen:hands"));
    assert!(
        root.visible_ids(&conn)?.is_none(),
        "the root hides nothing, which `None` states the strong way"
    );

    let james = Reader::pen("james").in_tree(&conn);
    let hidden = james.visible_ids(&conn)?.expect("a pen hides a row or two");
    let id: i64 = conn.query_row("SELECT id FROM memory WHERE key = 'stets-note'", [], |r| {
        r.get(0)
    })?;
    assert!(
        !hidden.contains(&id),
        "the SQL side refuses what `sees` refuses"
    );

    assert!(Reader::anonymous().sees("public"));
    Ok(())
}

#[test]
fn reading_a_descendants_private_row_is_recorded() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());
    tree(&conn);

    let parent = Reader::pen("james").in_tree(&conn);
    let _ = store::get(&conn, "stets-note")?;
    let hits = keys(&search::run(&conn, None, &parent, &all_q())?);
    drop(hits);

    assert!(
        parent.crosses("subtree:james"),
        "a share aimed at my branch is a boundary I cross"
    );
    assert!(
        !Reader::pen("stet").crosses("pen:stet"),
        "a pen does not cross its own boundary"
    );
    assert!(
        !Reader::pen("cline").in_tree(&conn).crosses("pen:stet"),
        "a sibling crosses nothing"
    );
    assert!(
        !parent.crosses("public"),
        "a public row is nobody's boundary"
    );
    Ok(())
}

#[test]
fn a_recorded_boundary_read_says_who_and_which_row() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let conn = seeded(dir.path());
    tree(&conn);

    store::audit::record(
        &conn,
        "james",
        Some("stets-note"),
        store::audit::Why::Subtree,
        None,
    );
    store::audit::record(
        &conn,
        "james",
        None,
        store::audit::Why::Search,
        Some("1 of 3 hit(s): stets-note"),
    );

    let rows = store::audit::recent(&conn, 10)?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].3, "search", "newest first");
    assert_eq!(rows[0].1, "james", "who looked");
    assert_eq!(rows[1].2.as_deref(), Some("stets-note"), "which row");
    assert_eq!(rows[1].3, "subtree", "why");
    assert_eq!(store::audit::count(&conn)?, 2);

    conn.execute(
        "UPDATE private_read SET at = datetime('now', '-400 days')",
        [],
    )
    .unwrap();
    assert_eq!(store::audit::prune(&conn, 180)?, 2);
    assert_eq!(store::audit::count(&conn)?, 0);
    Ok(())
}

#[test]
fn retention_runs_without_a_bell() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let paths = ellmephant::config::Paths::from_root(dir.path());

    {
        let conn = db::open(&paths)?;
        store::audit::record(
            &conn,
            "someone",
            Some("a-key"),
            store::audit::Why::Subtree,
            None,
        );
        conn.execute(
            "UPDATE private_read SET at = datetime('now', '-400 days')",
            [],
        )?;
        assert_eq!(store::audit::count(&conn)?, 1);
    }

    let conn = db::open(&paths)?;
    assert_eq!(
        store::audit::count(&conn)?,
        0,
        "an audit older than the window is dropped at open, with no bell to remember"
    );
    Ok(())
}
