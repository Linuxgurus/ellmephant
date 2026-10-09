//! Integration: the P4 write path against a real store file and real `vec0` tables.
//!
//! Runs offline — the embedder is the deterministic [`HashEmbedder`], wrapped so the test can
//! prove *when* a (re-)embed happens (and when it must not).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use common::{count, temp_store};
use llmphant::Result;
use llmphant::db;
use llmphant::embed::{Embedder, HashEmbedder};
use llmphant::index::vec;
use llmphant::store::{self, Memory};
use rusqlite::params;

const DIM: usize = 8;

/// `HashEmbedder` that counts `embed` calls, so re-embedding is observable.
struct CountingEmbedder {
    inner: HashEmbedder,
    calls: AtomicUsize,
}

impl CountingEmbedder {
    fn new() -> Self {
        Self {
            inner: HashEmbedder::new(DIM),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Embedder for CountingEmbedder {
    fn dim(&self) -> usize {
        self.inner.dim()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.embed(texts)
    }
}

/// A memory with an explicit key and the given body/tags.
fn memory(key: &str, title: &str, body: &str, tags: &[&str]) -> Memory {
    Memory {
        key: key.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        source: Some("tests/store.rs".to_string()),
        tags: tags.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn add_writes_row_one_vector_tags_and_links() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = CountingEmbedder::new();

    let id = store::add(
        &conn,
        &embedder,
        &memory(
            "anchor",
            "Anchor",
            "the quick brown fox sees [[ghost]]",
            &["K8s", "infra"],
        ),
    )
    .expect("add");

    // row
    let got = store::get(&conn, "anchor").unwrap().expect("row");
    assert_eq!(got.id, Some(id));
    assert_eq!(got.tags, vec!["infra".to_string(), "k8s".to_string()]);
    assert!(!got.doc_hash.is_empty());

    // one document vector
    assert_eq!(vec::count(&conn).unwrap(), 1);
    assert_eq!(vec::get(&conn, id).unwrap().unwrap().len(), DIM);

    // tags live on the row, normalised + sorted
    let tags: String = conn
        .query_row("SELECT tags FROM memory WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(tags, "infra,k8s");

    // links
    let target: String = conn
        .query_row(
            "SELECT to_key FROM memory_link WHERE from_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(target, "ghost");
}

#[test]
fn identical_readd_does_not_re_embed() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = CountingEmbedder::new();

    let m = memory("note", "Note", "same words here", &["k8s"]);
    let first = store::add(&conn, &embedder, &m).expect("add");
    assert_eq!(embedder.calls(), 1, "first add embeds the document");
    let vector_before = vec::get(&conn, first).unwrap();

    let again = store::add(&conn, &embedder, &m).expect("re-add");
    assert_eq!(again, first, "same key => same row");
    assert_eq!(embedder.calls(), 1, "unchanged row must not re-embed");
    assert_eq!(vec::get(&conn, first).unwrap(), vector_before);
    assert_eq!(count(&conn, "SELECT count(*) FROM memory"), 1);
}

#[test]
fn changed_body_reembeds_the_document() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = CountingEmbedder::new();

    let first = store::add(&conn, &embedder, &memory("note", "Note", "alpha", &[])).expect("add");
    let vector_before = vec::get(&conn, first).unwrap().unwrap();
    assert_eq!(embedder.calls(), 1);

    let second =
        store::add(&conn, &embedder, &memory("note", "Note", "beta gamma", &[])).expect("update");
    assert_eq!(second, first, "upsert keeps the row id");
    assert_eq!(embedder.calls(), 2, "a changed document re-embeds");
    assert_ne!(vec::get(&conn, first).unwrap().unwrap(), vector_before);
    assert_eq!(count(&conn, "SELECT count(*) FROM memory"), 1);
}

#[test]
fn delete_removes_row_and_every_derived_index() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = CountingEmbedder::new();

    store::add(
        &conn,
        &embedder,
        &memory("anchor", "Anchor", "lonely word [[ghost]]", &["k8s"]),
    )
    .expect("add");

    assert!(store::delete(&conn, "anchor").unwrap(), "row existed");
    assert_eq!(count(&conn, "SELECT count(*) FROM memory"), 0);
    assert_eq!(vec::count(&conn).unwrap(), 0, "vector gone");
    assert_eq!(count(&conn, "SELECT count(*) FROM memory_link"), 0);

    assert!(
        !store::delete(&conn, "anchor").unwrap(),
        "second delete is a no-op"
    );
}

#[test]
fn auto_key_gets_a_collision_suffix() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = CountingEmbedder::new();

    let mut auto = memory("", "", "", &[]);
    auto.title = "GitOps registry".into();

    store::add(&conn, &embedder, &auto).unwrap();
    store::add(&conn, &embedder, &auto).unwrap();

    let keys: Vec<String> = conn
        .prepare("SELECT key FROM memory ORDER BY key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(keys, vec!["gitops-registry", "gitops-registry-2"]);
}

#[test]
fn summaries_carry_key_title_and_tags_without_bodies() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = CountingEmbedder::new();
    store::add(&conn, &embedder, &memory("a", "A", "body a", &["k8s"])).unwrap();
    store::add(&conn, &embedder, &memory("b", "B", "body b", &["db"])).unwrap();

    let all = store::summaries(&conn).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].key, "a");
    assert_eq!(all[0].tags, vec!["k8s".to_string()]);

    let json = serde_json::to_string(&all).unwrap();
    assert!(
        !json.contains("\"body\""),
        "summaries must not carry bodies"
    );
}

#[test]
fn get_missing_is_none() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    assert!(store::get(&conn, "nope").unwrap().is_none());
}
