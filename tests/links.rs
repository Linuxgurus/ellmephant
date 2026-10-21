//! Integration: link-graph maintenance — `rename` and the dangling audit (P9).
//!
//! Runs offline against a real store file; the deterministic [`HashEmbedder`] stands in for
//! Qwen3. The sharpest assertions are that a rename rewrites the `[[…]]` tokens in *bodies*
//! and repoints the `memory_link` *edges* in the same transaction.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::{Embedder, HashEmbedder, VecSpec};
use ellmephant::store::{self, Memory, links, project};
use rusqlite::Connection;

const DIM: usize = 8;

fn memory(key: &str, body: &str) -> Memory {
    Memory {
        key: key.to_string(),
        title: key.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

fn seed(conn: &Connection, embedder: &HashEmbedder, key: &str, body: &str) {
    common::add_now(conn, embedder, &memory(key, body)).expect("add");
}

fn project_fixture(key: &str) -> project::Project {
    project::Project {
        key: key.to_string(),
        title: key.to_string(),
        ..Default::default()
    }
}

#[test]
fn rename_rewrites_bodies_and_repoints_edges() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);

    seed(&conn, &embedder, "target", "the target itself");
    seed(&conn, &embedder, "a", "see [[target]] first");
    seed(&conn, &embedder, "b", "also [[target]] here");

    let outcome = links::rename(&conn, "target", "renamed").expect("rename");
    assert_eq!(outcome.key, "renamed");
    assert_eq!(outcome.bodies, 2, "both bodies carried the token");
    assert_eq!(outcome.edges, 2, "both edges repointed");

    // The row moved.
    assert!(store::get(&conn, "target").unwrap().is_none());
    assert!(store::get(&conn, "renamed").unwrap().is_some());

    // Every body now points at the new key (no stale `[[target]]`).
    for key in ["a", "b"] {
        let body = store::get(&conn, key).unwrap().unwrap().body;
        assert!(body.contains("[[renamed]]"), "{key}: {body}");
        assert!(!body.contains("[[target]]"), "{key}: {body}");
    }
    let body = store::get(&conn, "renamed").unwrap().unwrap().body;
    assert_eq!(body, "the target itself", "the row's own body is untouched");

    // The edges repointed in lockstep; nothing is dangling.
    let keys: Vec<String> = conn
        .prepare("SELECT to_key FROM memory_link ORDER BY to_key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(keys, vec!["renamed", "renamed"]);
    assert!(links::dangling(&conn).unwrap().is_empty());
}

#[test]
fn rename_refuses_missing_and_colliding_keys() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);
    seed(&conn, &embedder, "one", "first");
    seed(&conn, &embedder, "two", "second");

    assert!(links::rename(&conn, "ghost", "new").is_err(), "missing old");
    let err = links::rename(&conn, "one", "two").unwrap_err();
    assert!(format!("{err}").contains("already exists"), "{err}");
    assert!(store::get(&conn, "one").unwrap().is_some(), "unchanged");
}

#[test]
fn rename_to_the_same_key_is_a_noop() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);
    seed(&conn, &embedder, "one", "body");

    let outcome = links::rename(&conn, "one", "one").expect("rename");
    assert_eq!((outcome.bodies, outcome.edges), (0, 0));
    assert!(store::get(&conn, "one").unwrap().is_some());
}

#[test]
fn rename_queues_a_new_vector_for_the_row_it_moves() {
    // `key` IS in the embed projection, so a rename moves the row's *document* — which is why the row
    // is QUEUED for a new vector, exactly as a write queues it. Once settled, `encode_hash` (and the
    // vector) describe the NEW key; otherwise the vector drifts until something else writes the row.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);
    // `memory()` uses the key as the title too, so the title stays put across the rename.
    seed(&conn, &embedder, "old-name", "the body");

    links::rename(&conn, "old-name", "new-name").expect("rename");
    common::settle(&conn, &embedder);

    // ★ THE VECTOR IS THE EVIDENCE (2026-10-21), because the row keeps no stamp: the stored vector
    // must be the one for the document rendered under the NEW key, not the old one.
    let id: i64 = conn
        .query_row("SELECT id FROM memory WHERE key = 'new-name'", [], |r| {
            r.get(0)
        })
        .expect("row moved");
    let stored = ellmephant::index::vec::get(&conn, id)
        .expect("get")
        .expect("the moved row has a vector");
    let fresh = embedder
        .embed_one(&ellmephant::embed::projection::embed_document(
            "new-name", "old-name", "the body",
        ))
        .expect("fresh vector");
    assert_eq!(
        stored, fresh,
        "the row's vector was rebuilt under its new key, not left describing the old one"
    );
    let stale = embedder
        .embed_one(&ellmephant::embed::projection::embed_document(
            "old-name", "old-name", "the body",
        ))
        .expect("stale vector");
    assert_ne!(stored, stale, "the stale, old-key vector was replaced");
}

#[test]
fn dangling_audit_groups_unresolved_targets() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    seed(&conn, &embedder, "present", "resolves");
    seed(&conn, &embedder, "a", "links [[ghost]] and [[present]]");
    seed(
        &conn,
        &embedder,
        "b",
        "also links [[ghost]] and [[phantom]]",
    );

    let dangling = links::dangling(&conn).expect("dangling");
    assert_eq!(dangling.len(), 2, "two unresolved targets: {dangling:?}");
    assert_eq!(dangling[0].to_key, "ghost");
    assert_eq!(
        dangling[0].from_keys,
        vec!["a".to_string(), "b".to_string()]
    );
    assert_eq!(dangling[1].to_key, "phantom");
    assert_eq!(dangling[1].from_keys, vec!["b".to_string()]);
}

#[test]
fn renaming_clears_a_dangling_edge() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);

    // `a` references a key that does not exist yet — a forward reference.
    seed(&conn, &embedder, "a", "forward [[later]]");
    assert_eq!(links::dangling(&conn).unwrap().len(), 1);

    // Renaming an unrelated memory into `later` resolves it (no edge repoint needed).
    seed(&conn, &embedder, "stub", "placeholder");
    links::rename(&conn, "stub", "later").expect("rename");
    assert!(links::dangling(&conn).unwrap().is_empty());
}

#[test]
fn a_link_to_a_project_resolves_and_is_not_rot() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    // A memory may link to a project — the `prj:` marker is what makes a key cross between the
    // two tables without either graph knowing about the other.
    ellmephant::store::project::upsert(
        &conn,
        &ellmephant::store::project::Project {
            key: "chronos".into(),
            title: "Chronos".into(),
            ..Default::default()
        },
        None,
    )
    .expect("project");
    seed(
        &conn,
        &embedder,
        "a",
        "works in [[prj:chronos]] and [[ghost]]",
    );

    let dangling = links::dangling(&conn).expect("dangling");
    assert_eq!(dangling.len(), 1, "only the ghost is rot: {dangling:?}");
    assert_eq!(dangling[0].to_key, "ghost");
}

#[test]
fn merge_into_repoints_every_reference_and_deletes_the_source() {
    // The whole point: a merge must not leave dangling links, or hygiene never happens.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);

    seed(&conn, &embedder, "old", "the deleted facts");
    // The caller folded it FIRST, so nothing is missing and this is nothing to integrate.
    seed(
        &conn,
        &embedder,
        "new",
        "the surviving facts, now carrying: the deleted facts",
    );
    seed(&conn, &embedder, "a", "see [[old]] first");
    seed(&conn, &embedder, "b", "also [[old]] here");
    let target_body = store::get(&conn, "new").unwrap().unwrap().body;

    let out = links::merge_into(&conn, "old", "new").expect("merge");
    assert_eq!(out.bodies, 2, "both referrers were rewritten");
    assert!(
        out.integrated.is_empty(),
        "a caller that folded first leaves nothing to integrate"
    );
    assert!(store::get(&conn, "old").unwrap().is_none(), "source gone");
    assert_eq!(
        store::get(&conn, "new").unwrap().unwrap().body,
        target_body,
        "the PROSE fold is the caller's: the merge itself edits no prose"
    );

    for key in ["a", "b"] {
        let body = store::get(&conn, key).unwrap().unwrap().body;
        assert!(body.contains("[[new]]"), "{key}: {body}");
        assert!(!body.contains("[[old]]"), "{key}: {body}");
    }
    assert!(
        links::dangling(&conn).unwrap().is_empty(),
        "nothing left dangling"
    );
    let to: Vec<String> = conn
        .prepare("SELECT to_key FROM memory_link ORDER BY to_key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(to, vec!["new", "new"], "the edges followed the bodies");
}

#[test]
fn merge_into_repoints_a_preload_but_not_a_project_link() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);
    seed(&conn, &embedder, "old", "deleted");
    seed(&conn, &embedder, "new", "survivor, now carrying: deleted");
    project::upsert(&conn, &project_fixture("p"), Some(&["old".to_string()])).expect("project");
    // A `[[prj:old]]` names a PROJECT, not the memory that shares its name.
    seed(&conn, &embedder, "pointer", "aimed at [[prj:old]]");

    let out = links::merge_into(&conn, "old", "new").expect("merge");
    assert_eq!(out.preloads, 1, "the project preload was repointed");
    let refs = project::preload(&conn, "p", project::Hydrate::None).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].key, "new", "the preload follows the merge");
    assert!(
        project::dangling(&conn).unwrap().is_empty(),
        "no preload rot after the merge"
    );

    let body = store::get(&conn, "pointer").unwrap().unwrap().body;
    assert!(
        body.contains("[[prj:old]]"),
        "a project link must not move: {body}"
    );
}

#[test]
fn merge_into_drops_the_self_link_the_rewrite_would_create() {
    // `new`'s own body points at `old`; rewritten, it would be a self-link, which the write path
    // never creates. The graph must keep that invariant.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);
    seed(&conn, &embedder, "old", "deleted");
    // `new` already carries `old`'s prose, so there is nothing to integrate.
    seed(&conn, &embedder, "new", "see [[old]] — and deleted");

    links::merge_into(&conn, "old", "new").expect("merge");
    let edges: i64 = conn
        .query_row("SELECT count(*) FROM memory_link", [], |r| r.get(0))
        .unwrap();
    assert_eq!(edges, 0, "a self-link carries no navigation value");
    assert!(links::dangling(&conn).unwrap().is_empty());
}

#[test]
fn merge_integrates_what_the_survivor_lacks_and_is_no_paste_gate() {
    // The integration: a merge APPENDS what the survivor lacks, verbatim. Layout is the caller's, so
    // an interleaved or re-ordered fold has nothing missing — and a line only the source carries is
    // folded in rather than refused, which is the case that used to be prose destroyed behind a 200.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);

    seed(&conn, &embedder, "old", "alpha fact.\nbeta fact.");
    seed(
        &conn,
        &embedder,
        "new",
        "survivor line.\nalpha fact.\nan inserted line.\nbeta fact.",
    );
    let out = links::merge_into(&conn, "old", "new").expect("an interleaved fold is a fold");
    assert!(out.integrated.is_empty(), "every line was already carried");
    assert!(
        store::get(&conn, "old").unwrap().is_none(),
        "source deleted"
    );
    let body = store::get(&conn, "new").unwrap().unwrap().body;
    assert!(
        body.contains("an inserted line."),
        "a caller that folded by hand keeps its layout: {body}"
    );

    // `twin` carries a line the survivor lacks. It is INTEGRATED — appended verbatim, nothing lost,
    // nothing paraphrased — while the line the survivor already carried is not duplicated. That is
    // the redundancy a merge exists to remove.
    seed(
        &conn,
        &embedder,
        "twin",
        "alpha fact.\nthe line only the twin carries.",
    );
    let before = store::get(&conn, "new").unwrap().unwrap().body;
    let out =
        links::merge_into(&conn, "twin", "new").expect("a missing line is integrated, not refused");
    assert_eq!(
        out.integrated,
        vec!["the line only the twin carries.".to_string()],
        "only the missing line is appended — a carried line is never duplicated"
    );
    let after = store::get(&conn, "new").unwrap().unwrap().body;
    assert_eq!(
        after,
        format!("{before}\nthe line only the twin carries."),
        "appended verbatim"
    );
    assert!(
        store::get(&conn, "twin").unwrap().is_none(),
        "the weaker memory is gone"
    );
    assert!(
        links::dangling(&conn).unwrap().is_empty(),
        "nothing left dangling"
    );
}

#[test]
fn merge_into_refuses_a_missing_target_and_a_self_merge() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);
    seed(&conn, &embedder, "a", "x");
    seed(&conn, &embedder, "b", "y");

    assert!(
        links::merge_into(&conn, "ghost", "a").is_err(),
        "source must exist"
    );
    assert!(
        links::merge_into(&conn, "a", "ghost").is_err(),
        "a merge into a typo must not destroy the source"
    );
    assert!(links::merge_into(&conn, "a", "a").is_err(), "self-merge");

    // A refused merge destroyed nothing.
    assert!(store::get(&conn, "a").unwrap().is_some());
    assert!(store::get(&conn, "b").unwrap().is_some());
}

/// ★ soma/ellmephant#48: the count said `dangling: 2` and no surface could name the two. A finder is
/// only worth having if its number AGREES with the count while its LEDGER disagrees — one missing
/// target with two referrers is one count and two rows, and that gap is exactly what the old count
/// hid. Falsifiable: if the rows were deduped to targets, the second assertion fails.
#[test]
fn the_dangling_finder_names_every_edge_not_every_target() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = VecSpec::of(&embedder);

    seed(&conn, &embedder, "kept", "a memory that stays");
    seed(&conn, &embedder, "first", "points at [[gone]]");
    seed(&conn, &embedder, "second", "also points at [[gone]]");
    seed(&conn, &embedder, "typo", "and at [[kept]] — one edge that resolves");

    let edges = links::dangling_edges(&conn).expect("finder");
    assert_eq!(edges.len(), 2, "one row per REFERRER: {edges:?}");
    assert_eq!(
        links::dangling_targets(&edges),
        1,
        "one distinct missing target — the number memory_stats counts"
    );
    assert_eq!(
        links::dangling_targets(&edges) as i64,
        store::stats::compute(&conn).expect("stats").dangling,
        "the finder and the count cannot disagree"
    );
    let referrers: Vec<&str> = edges.iter().map(|e| e.from_key.as_str()).collect();
    assert_eq!(referrers, vec!["first", "second"], "ordered, and named");

    // Repairing the target empties the finder, and the count with it.
    seed(&conn, &embedder, "gone", "the missing memory, written at last");
    assert!(
        links::dangling_edges(&conn).expect("finder").is_empty(),
        "no edges left"
    );
    assert_eq!(
        store::stats::compute(&conn).expect("stats").dangling,
        0,
        "and the count agrees"
    );
}
