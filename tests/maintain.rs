//! Integration: P9 maintenance — `reembed` (rebuild the vector index) and `tags prune`.
//!
//! Runs offline: the deterministic [`HashEmbedder`] stands in for Qwen3, and varying its
//! `dim` is exactly the model-swap the phase exists to handle.

mod common;

use common::{count, temp_store};
use llmphant::db;
use llmphant::embed::{Embedder, HashEmbedder, projection};
use llmphant::index::vec;
use llmphant::store::{self, Memory, MemoryType, maintain, tags};
use rusqlite::{Connection, params};

fn memory(key: &str, body: &str, tags_: &[&str]) -> Memory {
    Memory {
        key: key.to_string(),
        scope: "infra".to_string(),
        type_: MemoryType::Fact,
        title: key.to_string(),
        body: body.to_string(),
        tags: tags_.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn add(conn: &Connection, embedder: &dyn Embedder, key: &str, body: &str, tags_: &[&str]) -> i64 {
    store::add(conn, embedder, &memory(key, body, tags_)).expect("add")
}

fn meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT v FROM meta WHERE k = ?1", params![key], |r| {
        r.get(0)
    })
    .ok()
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
    assert!(report.rebuilt, "a dimension change recreates the table");
    assert_eq!((report.rows, report.dim), (2, 16));

    assert_eq!(vec::stored_dim(&conn).unwrap(), Some(16));
    assert_eq!(meta(&conn, "dim").as_deref(), Some("16"));
    assert_eq!(meta(&conn, "model").as_deref(), Some(wide.model_id()));
    assert_eq!(
        meta(&conn, "instruction").as_deref(),
        Some("test instruction")
    );

    assert_eq!(vec::count(&conn).unwrap(), 2, "every row re-indexed");
    for key in ["a", "b"] {
        let mem = store::get(&conn, key).unwrap().unwrap();
        let id = mem.id.unwrap();
        assert_eq!(vec::get(&conn, id).unwrap().unwrap().len(), 16);
        // The row bookkeeping now matches the new model.
        let dim: i64 = conn
            .query_row("SELECT dim FROM memory WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(dim, 16);
        let hash: String = conn
            .query_row("SELECT hash FROM memory WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .unwrap();
        let expected = store::row_hash(&wide, &projection::embed_text(&mem.title, &mem.body));
        assert_eq!(hash, expected);
    }
}

#[test]
fn reembed_same_dim_repairs_missing_vectors_and_is_stable() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    let a = add(&conn, &embedder, "a", "alpha", &[]);
    add(&conn, &embedder, "b", "beta", &[]);

    // Simulate a damaged derived index: drop one vector.
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
fn prune_drops_only_orphan_tags() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    add(&conn, &embedder, "keep", "kept", &["k8s"]);
    add(&conn, &embedder, "drop", "gone", &["k8s", "obsolete"]);

    // Deleting a row leaves its shared tags behind (SPEC §15 #6)...
    assert!(store::delete(&conn, "drop").unwrap());
    assert_eq!(count(&conn, "SELECT count(*) FROM tag"), 2);
    assert_eq!(tags::orphans(&conn).unwrap(), vec!["obsolete".to_string()]);

    // ...until pruned: the still-referenced tag survives.
    assert_eq!(tags::prune(&conn).unwrap(), 1);
    assert!(tags::orphans(&conn).unwrap().is_empty());
    let remaining: Vec<String> = conn
        .prepare("SELECT name FROM tag ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(remaining, vec!["k8s".to_string()]);
    assert_eq!(tags::prune(&conn).unwrap(), 0, "idempotent");
}
