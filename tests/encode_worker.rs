//! The encode worker (`ENCODE-WORKER.md`).
//!
//! A write queues its document; one worker indexes it; and the model call holds the store lock
//! **nowhere**. The first test is the regression test for the entire design — it does not measure a
//! duration, it asserts that a write completes while a model call is *provably* in flight, which is a
//! fact rather than an opinion.
//!
//! Runs offline: no weights, no network. Gated on the encoder, because the worker is: a store-only
//! build has nothing to run it with.

#![cfg(feature = "encoder")]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ellmephant::cli::session::Session;
use ellmephant::config::Config;
use ellmephant::db;
use ellmephant::embed::Embedder;
use ellmephant::error::Result;
use ellmephant::http::LockWatch;
use ellmephant::mcp::MemoryServer;
use ellmephant::model::Spec;
use ellmephant::store::Patch;

/// The fake embedder's width, and what the config has to echo (see the tests' `Config`).
const DIM: usize = 8;
const MODEL: &str = "gate";

/// An embedder that announces when it has entered the model call and then *parks* until released.
///
/// A sleeping fake would only make the assertion probable; a gate makes it certain, because the test
/// does not proceed until the embed is demonstrably underway.
///
/// Two details are load-bearing:
///
/// - only the first `parks` calls park, so the rest of a test runs at full speed; and
/// - the wait is a **`recv_timeout`, never a blocking `recv`**. If a regression puts the model call
///   back under the store lock, the write the test is timing will block — and with a blocking receive
///   the test would never reach the statement that releases the gate, so the whole suite would hang.
///   Bounding it means the write bounds out too, and the test **fails on its assertion** instead.
struct Gate {
    started: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    /// How many calls still park. The worker is single-threaded, so a load/store pair is enough.
    parks: AtomicUsize,
}

impl Gate {
    fn new(started: mpsc::Sender<()>, release: mpsc::Receiver<()>, parks: usize) -> Self {
        Self {
            started,
            release: Mutex::new(release),
            parks: AtomicUsize::new(parks),
        }
    }
}

impl Embedder for Gate {
    fn dim(&self) -> usize {
        DIM
    }
    fn model_id(&self) -> &str {
        MODEL
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let remaining = self.parks.load(Ordering::SeqCst);
        if remaining > 0 {
            self.parks.store(remaining - 1, Ordering::SeqCst);
            let _ = self.started.send(());
            // Bounded on purpose — see this struct's doc: a regression must fail a test, not hang one.
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3));
        }
        Ok(vec![vec![0.0; DIM]; texts.len()])
    }
}

fn patch(key: &str, body: &str) -> Patch {
    Patch {
        key: Some(key.to_string()),
        title: Some(key.to_string()),
        body: Some(body.to_string()),
        ..Default::default()
    }
}

fn memory(key: &str, body: &str) -> ellmephant::store::Memory {
    ellmephant::store::Memory {
        key: key.to_string(),
        title: key.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

/// A write must not wait for the model. Without this, the queue would be a queue nobody can see.
#[test]
fn a_write_returns_while_the_model_is_mid_embed() {
    let (_dir, paths) = common::temp_store();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    // The config is the write path's view of the vector space, so it has to describe the fake.
    let cfg = Config {
        model: MODEL.to_string(),
        dim: DIM,
        ..Config::default()
    };
    let spec = Spec::from_config(&cfg, &paths);
    let session = Session::open(paths.clone(), cfg, spec)
        .expect("open store")
        .with_embedder(Box::new(Gate::new(started_tx, release_rx, 1)));
    let shared = Arc::new(Mutex::new(session));
    let server = MemoryServer::with_watch(Arc::clone(&shared), Arc::new(LockWatch::new()));
    let mut worker = server.spawn_encode_worker(Duration::from_millis(5));

    // Queue a document. The worker picks it up, claims it, releases the store lock, and parks inside
    // the model call.
    shared
        .lock()
        .unwrap()
        .memory_merge(&patch("a", "alpha"))
        .expect("write a");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the worker reached the model call");

    // A second write, with a model call provably in flight. It takes the store lock, and must not wait
    // for the model to come back.
    let started = Instant::now();
    shared
        .lock()
        .unwrap()
        .memory_merge(&patch("b", "beta"))
        .expect("write b");
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_millis(100),
        "the write waited {waited:?} for a model call in flight: the encoder is under the store lock again"
    );

    // Let the model finish. Every later call runs straight through.
    release_tx.send(()).expect("release the model");

    let idle = db::open_read_only(&paths).expect("read the store");
    let deadline = Instant::now() + Duration::from_secs(5);
    while ellmephant::index::vec::count(&idle).expect("vector count") < 2 {
        assert!(
            Instant::now() < deadline,
            "the worker did not index both documents in time"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        ellmephant::store::encode::pending(&idle).expect("queue"),
        0,
        "the worker emptied the queue"
    );
    worker.stop();
}

/// The guard, end to end: a write that lands *while its own key is being embedded* must be kept, not
/// swallowed.
///
/// This is the failure with no symptom. A dequeue that ignored the hash would clear the entry the
/// worker claimed and take the newer text with it, leaving a vector describing text no longer in the
/// row — and nothing anywhere would say so. The proof that it did not happen is not an assertion about
/// a hash: it is that the worker **comes back to the key**.
#[test]
fn a_write_landing_mid_embed_is_kept_not_swallowed() {
    let (_dir, paths) = common::temp_store();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let cfg = Config {
        model: MODEL.to_string(),
        dim: DIM,
        ..Config::default()
    };
    let spec = Spec::from_config(&cfg, &paths);
    let session = Session::open(paths.clone(), cfg, spec)
        .expect("open store")
        // TWO parked calls: the first embeds `alpha` while the rewrite lands, and the second is the
        // worker coming back for `gamma` — the thing this test is actually asserting.
        .with_embedder(Box::new(Gate::new(started_tx, release_rx, 2)));
    let shared = Arc::new(Mutex::new(session));
    let server = MemoryServer::with_watch(Arc::clone(&shared), Arc::new(LockWatch::new()));
    let mut worker = server.spawn_encode_worker(Duration::from_millis(5));

    // `a` is claimed and parked inside its embed, holding the text "alpha".
    shared
        .lock()
        .unwrap()
        .memory_merge(&patch("a", "alpha"))
        .expect("write alpha");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the first embed started");

    // A rewrite of the SAME key lands mid-embed, superseding the queue entry: the row now says
    // "gamma", and the entry carries gamma's hash.
    shared
        .lock()
        .unwrap()
        .memory_merge(&patch("a", "gamma"))
        .expect("rewrite to gamma");

    // Let the first embed finish. Its dequeue must MISS, and the proof is that the worker returns.
    release_tx.send(()).expect("release the first embed");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the worker did not re-claim the key: the stale settle swallowed the rewrite");

    release_tx.send(()).expect("release the second embed");
    let idle = db::open_read_only(&paths).expect("read the store");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pending = ellmephant::store::encode::pending(&idle).expect("queue");
        if pending == 0 && ellmephant::index::vec::count(&idle).expect("vectors") == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the queue did not drain: {pending} left"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // And the vector describes the text actually in the row, which is the whole point.
    let expected = ellmephant::store::row_hash(
        &ellmephant::embed::VecSpec::new(MODEL, DIM),
        &ellmephant::embed::projection::embed_document("a", "a", "gamma"),
    );
    let stored: String = idle
        .query_row("SELECT doc_hash FROM memory WHERE key = 'a'", [], |r| {
            r.get(0)
        })
        .expect("doc_hash");
    assert_eq!(
        stored, expected,
        "the vector describes the CURRENT text, not the one the job first claimed"
    );
    worker.stop();
}

/// A store with nobody driving the queue keeps its documents: the relational rows are the truth and
/// the queue is only derived state, so a write is durable the moment it returns.
#[test]
fn a_queued_document_is_durable_before_it_is_ever_embedded() {
    let (_dir, paths) = common::temp_store();
    let conn = db::open(&paths).expect("open store");
    let spec = ellmephant::embed::VecSpec::new(MODEL, DIM);

    ellmephant::store::add(&conn, &spec, &memory("a", "alpha")).expect("write");
    assert_eq!(ellmephant::store::encode::pending(&conn).unwrap(), 1);
    assert_eq!(
        ellmephant::index::vec::count(&conn).unwrap(),
        0,
        "a write does not embed"
    );

    // Reopening the store — as a restart would — finds the work still owed, with no help from memory.
    drop(conn);
    let conn = db::open(&paths).expect("reopen");
    assert_eq!(ellmephant::store::encode::pending(&conn).unwrap(), 1);
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM memory WHERE key = 'a'", [], |r| {
            r.get(0)
        })
        .expect("row count");
    assert_eq!(rows, 1, "the row survived alongside it");

    // The same code the worker runs indexes it, and the queue empties.
    let embedder = ellmephant::embed::HashEmbedder::new(DIM);
    let spec = ellmephant::embed::VecSpec::of(&embedder);
    assert_eq!(
        ellmephant::store::encode::drain(&conn, &embedder, &spec, 16).expect("drain"),
        1
    );
    assert_eq!(ellmephant::store::encode::pending(&conn).unwrap(), 0);
    assert_eq!(ellmephant::index::vec::count(&conn).unwrap(), 1);
}
