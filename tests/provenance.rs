//! Integration: AUTHORSHIP vs THE WRITER — two provenance facts, and only one of them moves
//! (soma/ellmephant#107).
//!
//! Before this, `asserted_by` was one column stamped on every write, and tags create no version — so
//! re-filing a row silently moved its author to whoever filed it, with nothing in `memory_history` to
//! show the move. Measured on 2026-10-22 by a tag pass over 19 rows.

mod common;

use common::{count, temp_store};
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::store::{self, Memory};

const DIM: usize = 8;

/// A row whose author is `james`, created the way the store creates one.
fn seeded(conn: &rusqlite::Connection, embedder: &HashEmbedder, key: &str) -> i64 {
    let mem = Memory {
        key: key.to_string(),
        title: "authored by james".to_string(),
        body: "the facts, as james asserted them".to_string(),
        asserted_by: Some("james".to_string()),
        ..Default::default()
    };
    common::add_now(conn, embedder, &mem).expect("add memory").0
}

/// Register a pen, so a write has an identity that resolves. `cline` is the PARENT of `james` here on
/// purpose: these tests are about authorship versus the writer, and the writer must be a pen that may
/// write the row at all (soma/ellmephant#190 — before that fix the gate asked about the row's author,
/// so this fixture passed without a registry and proved the bug by accident).
fn pen(conn: &rusqlite::Connection, name: &str, parent: Option<&str>) {
    conn.execute(
        "INSERT OR IGNORE INTO pen (name, kind, parent_id)
         VALUES (?1, 'self', (SELECT id FROM pen WHERE name = ?2))",
        rusqlite::params![name, parent],
    )
    .expect("seed pen");
}

fn registry(conn: &rusqlite::Connection) {
    pen(conn, "cline", None);
    pen(conn, "james", Some("cline"));
}

fn provenance(conn: &rusqlite::Connection, id: i64) -> (String, Option<String>) {
    conn.query_row(
        "SELECT asserted_by, edited_by FROM memory WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .expect("provenance row")
}

#[test]
fn a_tag_only_write_moves_the_writer_and_not_the_author() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    registry(&conn);
    let id = seeded(&conn, &embedder, "handed-to-cline");

    // ★ THE REPRODUCTION: a tag pass by another pen. No title, no body, no version_note.
    let outcome = common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            key: Some("handed-to-cline".into()),
            tags: Some(vec!["pen:cline".into()]),
            asserted_by: Some("cline".into()),
            ..Default::default()
        },
    )
    .expect("tag-only write");
    assert!(!outcome.queued, "tags are filing: no vector is owed");

    let (asserted, last_written) = provenance(&conn, id);
    assert_eq!(
        asserted, "james",
        "authorship is write-once: a tag pass may not take the row over"
    );
    assert_eq!(
        last_written.as_deref(),
        Some("cline"),
        "the writer names itself, so the move is visible"
    );

    // Filing is still not meaning: no version is created, and none is needed — the two columns hold
    // both facts, which is why the bug is fixed without a history entry.
    assert_eq!(
        count(&conn, "SELECT count(*) FROM memory_version"),
        0,
        "a tag write still creates no version"
    );
}

#[test]
fn a_body_write_by_another_pen_keeps_the_author_and_versions_itself() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    registry(&conn);
    let id = seeded(&conn, &embedder, "edited-later");

    common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            key: Some("edited-later".into()),
            body: Some("the facts, corrected by cline".into()),
            asserted_by: Some("cline".into()),
            version_note: Some("correction".into()),
            ..Default::default()
        },
    )
    .expect("body write");

    let (asserted, last_written) = provenance(&conn, id);
    assert_eq!(
        asserted, "james",
        "the author of the document does not move"
    );
    assert_eq!(last_written.as_deref(), Some("cline"));
    assert_eq!(
        count(&conn, "SELECT count(*) FROM memory_version"),
        1,
        "a document change still snapshots what it replaced"
    );
}

#[test]
fn the_same_pen_writing_again_is_not_a_change_of_either_fact() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let id = seeded(&conn, &embedder, "james-again");

    common::merge_now(
        &conn,
        &embedder,
        &store::Patch {
            key: Some("james-again".into()),
            tags: Some(vec!["pen:james".into()]),
            asserted_by: Some("james".into()),
            ..Default::default()
        },
    )
    .expect("tag write");

    let (asserted, last_written) = provenance(&conn, id);
    assert_eq!(asserted, "james");
    assert_eq!(last_written.as_deref(), Some("james"));
}

#[test]
fn an_upgraded_store_backfills_the_last_writer_from_the_author() {
    // The migration's backfill is a CLAIM ABOUT HISTORY: before 0020 every write restamped
    // `asserted_by`, so the stored author IS the last writer. A row that predates the column must
    // not read as "never written".
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seeded(&conn, &embedder, "old-row");
    conn.execute("UPDATE memory SET edited_by = NULL", [])
        .unwrap();
    // Re-run the backfill the migration performs.
    conn.execute(
        "UPDATE memory SET edited_by = asserted_by WHERE edited_by IS NULL",
        [],
    )
    .unwrap();

    let (asserted, last_written) = provenance(&conn, 1);
    assert_eq!(asserted, "james");
    assert_eq!(last_written.as_deref(), Some("james"));
}
