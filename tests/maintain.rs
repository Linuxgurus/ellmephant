//! Integration: P9 maintenance — `reembed` (rebuild the vector index).
//!
//! Runs offline: the deterministic [`HashEmbedder`] stands in for Qwen3, and varying its
//! `dim` is exactly the model-swap the phase exists to handle.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::{Embedder, HashEmbedder, VecSpec, projection};
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
    let spec = VecSpec::of(&wide);
    let report = maintain::reembed(&conn, &spec, "test instruction").expect("reembed");
    assert!(report.rebuilt, "a dimension change recreates the tables");
    assert_eq!(
        (report.queued, report.dim),
        (2, 16),
        "every row is owed a vector at the new width"
    );
    // The vectors are the worker's job now, so settle exactly as it would.
    common::settle(&conn, &wide);

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
        let encode_hash: String = conn
            .query_row(
                "SELECT encode_hash FROM memory WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            encode_hash,
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

    let spec = VecSpec::of(&embedder);
    let report = maintain::reembed(&conn, &spec, "").expect("reembed");
    assert!(!report.rebuilt, "same dim keeps the table");
    // EVERY row, deliberately: with no per-row stamp there is no "nothing changed" to skip, and a
    // rebuild queues rather than deletes, so nothing here is destructive. `a` is repaired by the same
    // pass that re-embeds `b` for nothing.
    assert_eq!(report.queued, 2, "a rebuild is always-full");
    common::settle(&conn, &embedder);
    assert_eq!(vec::count(&conn).unwrap(), 2, "the missing vector is back");

    let before = vec::get(&conn, a).unwrap();
    let again = maintain::reembed(&conn, &spec, "").expect("reembed again");
    assert_eq!(
        again.queued, 2,
        "and it is full every time — that is the cost"
    );
    assert_eq!(
        vec::get(&conn, a).unwrap(),
        before,
        "the same text embeds to the same vector, so a rebuild is stable"
    );
}

/// A changed **projection** forces a full re-embed, and `encode_hash` cannot detect it: the hash covers the
/// model, the dimension and the text — never the recipe that assembled them. So every row is queued on
/// the fingerprint's say-so alone. Without that, a projection change would queue *nothing* and every
/// vector would keep describing the old recipe in silence — the drift the fingerprint exists to catch.
#[test]
fn reembed_requeues_everything_when_the_projection_changed() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    add(&conn, &embedder, "a", "alpha", &[]);
    add(&conn, &embedder, "b", "beta", &[]);
    let spec = VecSpec::of(&embedder);

    // A rebuild queues every row whatever the fingerprint says — the fingerprint no longer decides
    // WHETHER to rebuild, only whether the store is out of step afterwards.
    assert_eq!(maintain::reembed(&conn, &spec, "").unwrap().queued, 2);
    assert!(!vec::projection_stale(&conn).unwrap());

    // Now the projection moves underneath it, which is what a code change to `embed_document` looks
    // like from the store's side — the fingerprint is a hash of the projection's own output.
    conn.execute("UPDATE meta SET v = 'moved' WHERE k = 'projection'", [])
        .unwrap();
    assert!(vec::projection_stale(&conn).unwrap(), "precondition");

    let report = maintain::reembed(&conn, &spec, "").unwrap();
    assert!(!report.rebuilt, "the width did not change");
    assert_eq!(
        report.queued, 2,
        "a projection change owes EVERY row a vector"
    );
    common::settle(&conn, &embedder);
    assert_eq!(ellmephant::store::encode::pending(&conn).unwrap(), 0);
    assert!(
        !vec::projection_stale(&conn).unwrap(),
        "and the store is back in step"
    );
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

    maintain::reembed(&conn, &VecSpec::of(&embedder), "").expect("reembed");
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

    let report = maintain::reembed(&conn, &VecSpec::of(&embedder), "").expect("reembed");
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

    maintain::reembed(&conn, &VecSpec::of(&embedder), "").expect("reembed");
    assert_eq!(
        edge_count(&conn),
        0,
        "the code-span token is no longer an edge"
    );
}

/// The legacy index is **copied**, not re-embedded: `memory_vec` was keyed by `memory_id`, the new
/// table is partitioned by `type` with an opaque `vector_id`, and the dimension did not change — so
/// the vectors move by `INSERT..SELECT` and the encoder is never called.
///
/// This is the migration that makes the whole change cheap: ~300 vectors at ~11s each would be an
/// hour of work, and a copy is milliseconds (measured: the copy answers a KNN at distance 0.0).
#[test]
fn the_legacy_index_is_copied_not_reembedded() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    let spec = VecSpec::of(&embedder);
    let id = store::add(&conn, &spec, &memory("a", "alpha", &[]))
        .expect("write")
        .0;

    // Put the store back into the OLD shape by hand: no partitioned table, a `memory_vec` holding one
    // vector under the memory's own id.
    conn.execute_batch(
        "DROP TABLE IF EXISTS document_vec;
         DROP TABLE IF EXISTS document_vec_deleted;
         CREATE VIRTUAL TABLE memory_vec USING vec0(
           memory_id INTEGER PRIMARY KEY, embedding float[4] distance_metric=cosine);",
    )
    .expect("legacy shape");
    conn.execute(
        "INSERT INTO memory_vec(memory_id, embedding) VALUES(?1, ?2)",
        params![id, vec::to_blob(&[1.0, 0.0, 0.0, 0.0])],
    )
    .expect("legacy vector");

    // The first `ensure` after the upgrade is the migration.
    vec::ensure(&conn, 4).expect("migrate");

    assert_eq!(
        vec::count(&conn).expect("count"),
        1,
        "the vector came across"
    );
    assert!(
        vec::get(&conn, id).expect("get").is_some(),
        "under the same id, because a memory's id is its vector id"
    );
    assert_eq!(
        vec::keys_of(&conn, "memory").expect("keys"),
        vec!["a"],
        "and it knows which document it belongs to"
    );
    let legacy: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'memory_vec'",
            [],
            |r| r.get(0),
        )
        .expect("lookup");
    assert_eq!(legacy, 0, "the old table is gone once copied");
}

/// A delete MOVES the vector into the tombstone index — it is not erased.
///
/// James, 2026-10-21: an embedding is semantic, not the source of truth, so deleting a document must
/// not erase what the store knew about it. The tombstone is a *separate table*, which is what makes
/// "unreachable by any normal search" structural rather than a filter someone must remember.
#[test]
fn a_delete_moves_the_vector_to_the_tombstone_index() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    let spec = VecSpec::of(&embedder);
    let (id, _) = store::add(&conn, &spec, &memory("gone", "to be deleted", &[])).expect("write");
    common::settle(&conn, &embedder);
    assert!(vec::get(&conn, id).expect("get").is_some(), "indexed");

    assert!(store::delete(&conn, "gone").expect("delete"));

    assert!(
        vec::get(&conn, id).expect("get").is_none(),
        "no normal search can reach it"
    );
    assert_eq!(
        vec::count_deleted(&conn).expect("tombstones"),
        1,
        "but the store still knows it existed"
    );
    assert_eq!(vec::count(&conn).expect("live"), 0);
}

/// The half-copied state, constructed on purpose — stet's test, and the one that catches the bug this
/// code had: `execute_batch` autocommits each statement, so the copy could land while the DROP did
/// not, and then `ensure` failed on every open and the store never started again.
///
/// The contract: the copy and the drop are ONE transaction, and a partial state (rows in BOTH tables)
/// COMPLETES rather than colliding. Nothing is duplicated, nothing is overwritten with legacy data.
#[test]
fn a_half_copied_legacy_index_completes_without_duplicating() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    let spec = VecSpec::of(&embedder);
    let a = store::add(&conn, &spec, &memory("a", "alpha", &[]))
        .expect("write")
        .0;
    let b = store::add(&conn, &spec, &memory("b", "beta", &[]))
        .expect("write")
        .0;

    // Old shape, with two vectors in it.
    conn.execute_batch(
        "DROP TABLE IF EXISTS document_vec;
         DROP TABLE IF EXISTS document_vec_deleted;
         CREATE VIRTUAL TABLE memory_vec USING vec0(
           memory_id INTEGER PRIMARY KEY, embedding float[4] distance_metric=cosine);
         INSERT INTO memory_vec(memory_id, embedding) VALUES
           (1, X'0000803F000000000000000000000000'),
           (2, X'0000803F000000000000000000000000');",
    )
    .expect("legacy shape");

    // The crash window, reproduced: the copy landed for one row, the drop did not.
    vec::ensure(&conn, 4).expect("first ensure migrates");
    conn.execute_batch(
        "DROP TABLE document_vec_deleted;
         CREATE VIRTUAL TABLE document_vec_deleted USING vec0(
           vector_id INTEGER PRIMARY KEY, embedding float[4] distance_metric=cosine,
           type text PARTITION KEY, +key TEXT, +deleted_at TEXT);
         CREATE VIRTUAL TABLE memory_vec USING vec0(
           memory_id INTEGER PRIMARY KEY, embedding float[4] distance_metric=cosine);
         INSERT INTO memory_vec(memory_id, embedding) VALUES (1, X'0000803F000000000000000000000000');",
    )
    .expect("re-create the legacy table with an already-copied id");

    // A store that already holds both must still open, complete, and not duplicate.
    let done = vec::ensure(&conn, 4);
    assert!(done.is_ok(), "a half-copied index must complete: {done:?}");
    assert_eq!(
        vec::count(&conn).expect("count"),
        2,
        "no duplicates, no loss"
    );
    let rows: Vec<i64> = {
        let mut st = conn
            .prepare("SELECT vector_id FROM document_vec ORDER BY vector_id")
            .expect("prepare");
        st.query_map([], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<i64>>>()
            .expect("rows")
    };
    assert_eq!(rows, vec![a, b], "each id appears exactly once");
    let legacy: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'memory_vec'",
            [],
            |r| r.get(0),
        )
        .expect("lookup");
    assert_eq!(legacy, 0, "and the legacy table is gone at the end");
}
