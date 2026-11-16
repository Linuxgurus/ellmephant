//! Integration: `memory_append` — continuing a subject in one call (the cheap verb).
//!
//! The point of the verb is economic: a pen mid-work must be able to add a line WITHOUT reading and
//! resending the whole body, or it will write a second row instead. These tests pin the refusals that
//! keep it from becoming a back door to creating, and the bookkeeping a body write owes: a version,
//! the edges the new line implies, and an embed job.

mod common;

use common::temp_store;

use ellmephant::embed::HashEmbedder;
use ellmephant::store::{self, Memory};

const DIM: usize = 8;

fn add(conn: &rusqlite::Connection, embedder: &HashEmbedder, key: &str, author: &str, body: &str) {
    let mem = Memory {
        key: key.to_string(),
        title: format!("the subject of {key}"),
        body: body.to_string(),
        asserted_by: Some(author.to_string()),
        ..Default::default()
    };
    common::add_now(conn, embedder, &mem).expect("add memory");
}

fn body_of(conn: &rusqlite::Connection, key: &str) -> String {
    store::get(conn, key).unwrap().expect("row").body
}

#[test]
fn one_call_appends_one_line_and_keeps_a_version() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    add(&conn, &embedder, "subject", "cline", "the first fact");

    let out = store::append::append(&conn, "subject", "  and the second  ", Some("cline"), false)
        .expect("append");
    assert!(out.appended);
    assert_eq!(out.line, "and the second");
    assert_eq!(out.version, Some(1), "the state it replaced is history");
    assert_eq!(body_of(&conn, "subject"), "the first fact\nand the second");

    // The superseded state is readable by address.
    let versions: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_version WHERE type = 'memory' AND key = 'subject'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(versions, 1);
}

#[test]
fn the_same_line_twice_is_a_retry_not_a_second_fact() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    add(&conn, &embedder, "subject", "cline", "the fact");

    let once = store::append::append(&conn, "subject", "another fact", Some("cline"), false)
        .expect("append");
    assert!(once.appended);
    // Same words, different whitespace: still the same line.
    let twice = store::append::append(&conn, "subject", "another   fact", Some("cline"), false)
        .expect("append");
    assert!(!twice.appended, "a retry writes nothing");
    assert_eq!(twice.version, None);
    assert_eq!(body_of(&conn, "subject"), "the fact\nanother fact");
}

#[test]
fn a_missing_key_is_refused_and_names_upsert() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let err = store::append::append(&conn, "no-such-subject", "a line", Some("cline"), false)
        .expect_err("refused");
    let text = format!("{err}");
    assert!(text.contains("memory_upsert"), "{text}");
    assert!(text.contains("CONTINUES"), "{text}");
    assert!(
        store::get(&conn, "no-such-subject").unwrap().is_none(),
        "no row was created"
    );
}

#[test]
fn two_lines_in_one_call_are_refused() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    add(&conn, &embedder, "subject", "cline", "the fact");
    let err = store::append::append(&conn, "subject", "one\nanother", Some("cline"), false)
        .expect_err("refused");
    assert!(format!("{err}").contains("ONE line"), "{err}");
    assert!(
        store::append::append(&conn, "subject", "   ", Some("cline"), false).is_err(),
        "empty refused"
    );
}

#[test]
fn it_cannot_change_the_title_tags_or_audience() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    add(&conn, &embedder, "subject", "cline", "the fact");
    store::merge(
        &conn,
        &store::Patch {
            key: Some("subject".into()),
            acted_by: Some("cline".into()),
            tags: Some(vec!["rules".into()]),
            audience: Some("pen:cline".into()),
            ..Default::default()
        },
    )
    .expect("tag + audience");

    store::append::append(&conn, "subject", "one more fact", Some("cline"), false).expect("append");

    let mem = store::get(&conn, "subject").unwrap().expect("row");
    assert_eq!(mem.title, "the subject of subject");
    assert_eq!(mem.tags, vec!["rules".to_string()]);
    assert_eq!(mem.audience, "pen:cline");
    assert_eq!(
        mem.edited_by.as_deref(),
        Some("cline"),
        "the append is named"
    );
}

#[test]
fn a_peer_may_not_append_to_another_pens_row() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    for (name, parent) in [
        ("estate", None),
        ("james", Some("estate")),
        ("stet", Some("james")),
        ("cline", Some("james")),
    ] {
        conn.execute(
            "INSERT OR IGNORE INTO pen (name, kind, parent_id) VALUES (?1, 'self', (SELECT id FROM pen WHERE name = ?2))",
            rusqlite::params![name, parent],
        )
        .expect("seed pen");
    }
    add(&conn, &embedder, "stets-subject", "stet", "stet's fact");

    let err = store::append::append(
        &conn,
        "stets-subject",
        "a line from a sibling",
        Some("cline"),
        false,
    )
    .expect_err("a sibling is refused");
    assert!(format!("{err}").contains("may not write"), "{err}");
    assert_eq!(body_of(&conn, "stets-subject"), "stet's fact");
}

#[test]
fn a_link_in_the_appended_line_becomes_an_edge() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    add(&conn, &embedder, "subject", "cline", "the fact");
    add(&conn, &embedder, "neighbour", "cline", "another subject");

    store::append::append(
        &conn,
        "subject",
        "see [[neighbour]] for the rest",
        Some("cline"),
        false,
    )
    .expect("append");

    let edges: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_link WHERE to_key = 'neighbour' AND to_type = 'memory'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        edges, 1,
        "the new line's token is a live edge, not just text"
    );
}

#[test]
fn a_protected_row_keeps_its_lease_gate() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let mem = Memory {
        key: "pinned".into(),
        title: "pinned".into(),
        body: "the first fact".into(),
        asserted_by: Some("cline".into()),
        tags: vec![store::PROTECTED.to_string()],
        ..Default::default()
    };
    common::add_now(&conn, &embedder, &mem).expect("add");

    // ★ Measured hole in the first head of #229: `may_write` alone let an append write a protected row
    // that `upsert` refuses without a lease.
    let err = store::append::append(&conn, "pinned", "sneaked line", Some("cline"), false)
        .expect_err("a protected row needs a lease");
    assert!(format!("{err}").contains("PROTECTED"), "{err}");
    assert_eq!(body_of(&conn, "pinned"), "the first fact");

    store::claim::acquire(&conn, "pinned", "cline", Some(60)).expect("take the lease");
    let out = store::append::append(&conn, "pinned", "a leased line", Some("cline"), false)
        .expect("under a live lease it writes");
    assert!(out.appended);
}

#[test]
fn three_terse_appends_may_not_build_the_body_upsert_refuses() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    add(&conn, &embedder, "runs", "cline", "the subject");

    // ★ Measured hole in the first head of #229: a pen that cannot write a play-by-play in one call
    // could write one in three, because the gate only inspected `upsert`.
    store::append::append(
        &conn,
        "runs",
        "2026-11-09T02:17Z probe ran, 3 rows",
        Some("cline"),
        false,
    )
    .expect("first");
    store::append::append(
        &conn,
        "runs",
        "2026-11-09T05:17Z probe ran, 3 rows",
        Some("cline"),
        false,
    )
    .expect("second");

    let err = store::append::append(
        &conn,
        "runs",
        "2026-11-09T11:17Z probe ran, 3 rows",
        Some("cline"),
        false,
    )
    .expect_err("the RESULT is a run record, whatever the line was");
    assert!(
        format!("{err}").to_lowercase().contains("run record"),
        "{err}"
    );

    // The deliberate override is the same one `upsert` offers.
    let forced = store::append::append(
        &conn,
        "runs",
        "2026-11-09T11:17Z probe ran, 3 rows",
        Some("cline"),
        true,
    )
    .expect("force writes it");
    assert!(forced.appended);
}

#[test]
fn case_is_part_of_the_fact() {
    let (_dir, paths) = temp_store();
    let conn = ellmephant::db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    add(
        &conn,
        &embedder,
        "routes",
        "cline",
        "the write door is documented here",
    );

    store::append::append(
        &conn,
        "routes",
        "POST /memory is the write door",
        Some("cline"),
        false,
    )
    .expect("first");
    // ★ Measured hole in the first head of #229: lowercasing the retry check silently dropped this.
    let second = store::append::append(
        &conn,
        "routes",
        "post /memory is the write door",
        Some("cline"),
        false,
    )
    .expect("append");
    assert!(second.appended, "a case-distinct line is a different line");
    assert!(body_of(&conn, "routes").contains("post /memory is the write door"));
}
