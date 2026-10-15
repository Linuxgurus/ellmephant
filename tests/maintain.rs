//! Integration: P9 maintenance — `reembed` (rebuild the vector index).
//!
//! Runs offline: the deterministic [`HashEmbedder`] stands in for Qwen3, and varying its
//! `dim` is exactly the model-swap the phase exists to handle.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::{Embedder, HashEmbedder, projection};
use ellmephant::index::vec;
use ellmephant::store::{self, Memory, links, maintain};
use rusqlite::{Connection, params};

fn memory(key: &str, body: &str, tags_: &[&str]) -> Memory {
    Memory {
        key: key.to_string(),
        title: key.to_string(),
        body: body.to_string(),
        tags: tags_.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn add(conn: &Connection, embedder: &dyn Embedder, key: &str, body: &str, tags_: &[&str]) -> i64 {
    common::add_now(conn, embedder, &memory(key, body, tags_))
        .expect("add")
        .0
}

fn meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT v FROM meta WHERE k = ?1", params![key], |r| {
        r.get(0)
    })
    .ok()
}

fn edge_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM memory_link", [], |r| r.get(0))
        .expect("count edges")
}

#[test]
fn reembed_with_a_different_dim_recreates_vec_and_updates_meta() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let small = HashEmbedder::new(8);
    add(&conn, &small, "a", "alpha beta", &[]);
    add(&conn, &small, "b", "gamma delta", &[]);
    assert_eq!(vec::stored_dim(&conn).unwrap(), Some(8));

    // Swap to a wider model (a dim change `index::vec::ensure` would refuse).
    let wide = HashEmbedder::new(16);
    let report = maintain::reembed(&conn, &wide, "test instruction").expect("reembed");
    assert!(report.rebuilt, "a dimension change recreates the tables");
    assert_eq!((report.rows, report.dim), (2, 16));

    assert_eq!(vec::stored_dim(&conn).unwrap(), Some(16));
    assert_eq!(meta(&conn, "dim").as_deref(), Some("16"));
    assert_eq!(meta(&conn, "model").as_deref(), Some(wide.model_id()));
    assert_eq!(
        meta(&conn, "instruction").as_deref(),
        Some("test instruction")
    );

    assert_eq!(vec::count(&conn).unwrap(), 2, "every row indexed");
    for key in ["a", "b"] {
        let mem = store::get(&conn, key).unwrap().unwrap();
        let id = mem.id.unwrap();
        assert_eq!(vec::get(&conn, id).unwrap().unwrap().len(), 16);
        // The row bookkeeping now matches the new model.
        let doc_hash: String = conn
            .query_row(
                "SELECT doc_hash FROM memory WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            doc_hash,
            store::row_hash(
                &ellmephant::embed::VecSpec::of(&wide),
                &projection::embed_document(&mem.key, &mem.title, &mem.body)
            )
        );
    }
}

#[test]
fn reembed_same_dim_repairs_missing_vectors_and_is_stable() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    let a = add(&conn, &embedder, "a", "alpha", &[]);
    add(&conn, &embedder, "b", "beta", &[]);

    // Simulate a damaged derived index: drop one row's vector.
    vec::delete(&conn, a).unwrap();
    assert_eq!(vec::count(&conn).unwrap(), 1);

    let report = maintain::reembed(&conn, &embedder, "").expect("reembed");
    assert!(!report.rebuilt, "same dim keeps the table");
    assert_eq!(report.rows, 2);
    assert_eq!(vec::count(&conn).unwrap(), 2, "the missing vector is back");

    let before = vec::get(&conn, a).unwrap();
    maintain::reembed(&conn, &embedder, "").expect("reembed again");
    assert_eq!(vec::get(&conn, a).unwrap(), before, "vectors are stable");
}

#[test]
fn reembed_records_the_projection_fingerprint() {
    // A change to what the projection emits would leave every stored vector describing the old
    // recipe; the fingerprint in `meta` is what makes that drift detectable.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    add(&conn, &embedder, "a", "alpha beta", &[]);

    // The write recorded the baseline (`index::vec::ensure`), so nothing is stale.
    assert!(
        meta(&conn, "projection").is_some(),
        "the baseline is recorded on first use"
    );
    assert!(!vec::projection_stale(&conn).unwrap());

    maintain::reembed(&conn, &embedder, "").expect("reembed");
    assert_eq!(
        meta(&conn, "projection").as_deref(),
        Some(projection::fingerprint().as_str()),
        "reembed writes the current projection fingerprint"
    );
    assert!(!vec::projection_stale(&conn).unwrap());
}

#[test]
fn reembed_rebuilds_the_link_graph_from_the_bodies() {
    // Edges are computed at WRITE time, so a parser change does not reach an existing body on its
    // own — the stored graph can disagree with the rows. The full rebuild re-parses every body.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    let a = add(&conn, &embedder, "a", "points at [[b]]", &[]);
    add(&conn, &embedder, "b", "the target", &[]);
    assert_eq!(edge_count(&conn), 1);

    // Simulate a stale edge an older parser would have left: one the current body does not imply.
    conn.execute(
        "INSERT INTO memory_link (from_id, to_key, to_type, rel) VALUES (?1, 'ghost', 'memory', 'see')",
        params![a],
    )
    .expect("stale edge");
    assert_eq!(edge_count(&conn), 2, "[[b]] plus the stale [[ghost]]");

    let report = maintain::reembed(&conn, &embedder, "").expect("reembed");
    assert_eq!(report.linked, 2, "both rows were re-parsed");
    assert_eq!(
        edge_count(&conn),
        1,
        "the stale edge is gone; [[b]] remains"
    );
    assert!(links::dangling(&conn).unwrap().is_empty());
}

#[test]
fn reparse_applies_the_code_span_rule_to_an_existing_body() {
    // The backtick escape is a parser change; a full rebuild is how a body written before it
    // catches up (the edge it wrongly created is not removed by rewriting the row alone).
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    // A body that only *documents* the syntax creates no edge under the current parser...
    let a = add(&conn, &embedder, "doc", "write `[[key]]` to link", &[]);
    assert_eq!(edge_count(&conn), 0);

    // ...but one left by the old parser did. Rebuild and it is gone.
    conn.execute(
        "INSERT INTO memory_link (from_id, to_key, to_type, rel) VALUES (?1, 'key', 'memory', 'see')",
        params![a],
    )
    .expect("legacy edge");
    assert_eq!(edge_count(&conn), 1);

    maintain::reembed(&conn, &embedder, "").expect("reembed");
    assert_eq!(
        edge_count(&conn),
        0,
        "the code-span token is no longer an edge"
    );
}
