//! Integration: the P4 write path against a real store file and real `vec0` tables.
//!
//! Runs offline — the embedder is the deterministic [`HashEmbedder`], wrapped so the test can
//! prove *when* a (re-)embed happens (and when it must not).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use common::{count, temp_store};
use ellmephant::Result;
use ellmephant::db;
use ellmephant::embed::{Embedder, HashEmbedder};
use ellmephant::index::vec;
use ellmephant::store::{self, Memory};
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

    let (id, _) = common::add_now(
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
    let (first, _) = common::add_now(&conn, &embedder, &m).expect("add");
    assert_eq!(embedder.calls(), 1, "first add embeds the document");
    let vector_before = vec::get(&conn, first).unwrap();

    let (again, _) = common::add_now(&conn, &embedder, &m).expect("re-add");
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

    let (first, _) =
        common::add_now(&conn, &embedder, &memory("note", "Note", "alpha", &[])).expect("add");
    let vector_before = vec::get(&conn, first).unwrap().unwrap();
    assert_eq!(embedder.calls(), 1);

    let (second, _) = common::add_now(&conn, &embedder, &memory("note", "Note", "beta gamma", &[]))
        .expect("update");
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

    common::add_now(
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

    common::add_now(&conn, &embedder, &auto).unwrap();
    common::add_now(&conn, &embedder, &auto).unwrap();

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
    common::add_now(&conn, &embedder, &memory("a", "A", "body a", &["k8s"])).unwrap();
    common::add_now(&conn, &embedder, &memory("b", "B", "body b", &["db"])).unwrap();

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

#[test]
fn visit_and_useful_counters_bump_by_key() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = CountingEmbedder::new();
    common::add_now(&conn, &embedder, &memory("a", "A", "body a", &["k8s"])).unwrap();

    // A fresh row starts at zero.
    let got = store::get(&conn, "a").unwrap().unwrap();
    assert_eq!((got.visits, got.times_useful), (0, 0));

    // Reads count; each call returns the new total.
    assert_eq!(store::record_visit(&conn, "a").unwrap(), 1);
    assert_eq!(store::record_visit(&conn, "a").unwrap(), 2);
    // `times_useful` is independent of a fetch.
    assert_eq!(store::record_useful(&conn, "a").unwrap(), 1);

    let got = store::get(&conn, "a").unwrap().unwrap();
    assert_eq!((got.visits, got.times_useful), (2, 1));

    // An unknown key is an error, not a silent zero.
    assert!(store::record_visit(&conn, "ghost").is_err());
    assert!(store::record_useful(&conn, "ghost").is_err());
}

#[test]
fn record_useful_many_sweeps_known_keys_and_reports_the_rest() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = CountingEmbedder::new();
    common::add_now(&conn, &embedder, &memory("a", "A", "body a", &["k8s"])).unwrap();
    common::add_now(&conn, &embedder, &memory("b", "B", "body b", &["k8s"])).unwrap();

    let keys = vec!["a".to_string(), "ghost".to_string(), "b".to_string()];
    let (upvoted, missing) = store::record_useful_many(&conn, &keys).unwrap();

    // Known keys in input order with their new totals; the unknown one is reported, not fatal.
    assert_eq!(upvoted, vec![("a".to_string(), 1), ("b".to_string(), 1)]);
    assert_eq!(missing, vec!["ghost".to_string()]);

    // A second sweep stacks on the first.
    let again = vec!["a".to_string()];
    let (upvoted, missing) = store::record_useful_many(&conn, &again).unwrap();
    assert_eq!(upvoted, vec![("a".to_string(), 2)]);
    assert!(missing.is_empty());
}

#[test]
fn upsert_preserves_counters() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = CountingEmbedder::new();
    common::add_now(&conn, &embedder, &memory("a", "A", "body a", &["k8s"])).unwrap();
    store::record_visit(&conn, "a").unwrap();
    store::record_useful(&conn, "a").unwrap();

    // Re-writing the same key must not reset its telemetry.
    common::add_now(&conn, &embedder, &memory("a", "A2", "body a2", &["k8s"])).unwrap();
    let got = store::get(&conn, "a").unwrap().unwrap();
    assert_eq!(got.title, "A2");
    assert_eq!((got.visits, got.times_useful), (1, 1));
}

#[test]
fn upsert_resets_the_coherence_verdict() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = CountingEmbedder::new();
    common::add_now(&conn, &embedder, &memory("a", "A", "body", &[])).expect("add");

    // A fresh row is unjudged.
    assert_eq!(store::get(&conn, "a").unwrap().unwrap().coherent, None);

    assert!(store::set_coherent(&conn, "a", Some(true)).unwrap());
    assert_eq!(
        store::get(&conn, "a").unwrap().unwrap().coherent,
        Some(true)
    );

    // Unknown key is a no-op, not an error.
    assert!(!store::set_coherent(&conn, "ghost", Some(false)).unwrap());

    // An upsert resets the verdict (even a byte-identical rewrite re-opens review).
    common::add_now(&conn, &embedder, &memory("a", "A", "body", &[])).expect("re-add");
    assert_eq!(store::get(&conn, "a").unwrap().unwrap().coherent, None);

    // Setting it again does not re-embed (coherent is not part of the document).
    let calls = embedder.calls();
    store::set_coherent(&conn, "a", Some(false)).unwrap();
    assert_eq!(embedder.calls(), calls, "marking must not embed");
}
