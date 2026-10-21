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
        .query_row("SELECT encode_hash FROM memory WHERE key = 'a'", [], |r| {
            r.get(0)
        })
        .expect("encode_hash");
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

/// A soak: many writers, one worker, and a store that must end up **consistent**, not merely quiet.
///
/// Convergence alone would pass even if the queue silently dropped work. So this asserts the stronger
/// thing: every row has a vector, and every row's `encode_hash` describes the text the row actually
/// holds. A swallowed write, or a stale settle, shows up as drift — an index or a hash describing text
/// that is no longer there — which is exactly the failure with no other symptom.
///
/// All writers also rewrite ONE shared key, because that is where a dequeue that ignored the hash
/// loses a write for good.
#[test]
fn many_writers_and_one_worker_converge_without_drift() {
    const WRITERS: usize = 6;
    const PER_WRITER: usize = 40;

    let (_dir, paths) = common::temp_store();
    let cfg = Config {
        model: MODEL.to_string(),
        dim: DIM,
        ..Config::default()
    };
    let spec = Spec::from_config(&cfg, &paths);
    let session = Session::open(paths.clone(), cfg, spec)
        .expect("open store")
        .with_embedder(Box::new(ellmephant::embed::HashEmbedder::new(DIM)));
    let shared = Arc::new(Mutex::new(session));
    let server = MemoryServer::with_watch(Arc::clone(&shared), Arc::new(LockWatch::new()));
    let mut worker = server.spawn_encode_worker(Duration::from_millis(1));

    let mut writers = Vec::new();
    for w in 0..WRITERS {
        let shared = Arc::clone(&shared);
        writers.push(std::thread::spawn(move || {
            for n in 0..PER_WRITER {
                shared
                    .lock()
                    .unwrap()
                    .memory_merge(&patch(&format!("w{w}-{n}"), &format!("body {w} {n}")))
                    .expect("write");
                shared
                    .lock()
                    .unwrap()
                    .memory_merge(&patch("contended", &format!("writer {w} pass {n}")))
                    .expect("contended write");
            }
        }));
    }
    for writer in writers {
        writer.join().expect("a writer thread");
    }

    let idle = db::open_read_only(&paths).expect("read the store");
    let deadline = Instant::now() + Duration::from_secs(30);
    while ellmephant::store::encode::pending(&idle).expect("queue") > 0 {
        assert!(Instant::now() < deadline, "the queue never drained");
        std::thread::sleep(Duration::from_millis(20));
    }
    worker.stop();

    let spec = ellmephant::embed::VecSpec::new(MODEL, DIM);
    let rows: Vec<(String, String, String, String)> = {
        let mut stmt = idle
            .prepare("SELECT key, title, body, encode_hash FROM memory ORDER BY key")
            .expect("prepare");
        let mapped = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("query");
        mapped.collect::<rusqlite::Result<Vec<_>>>().expect("rows")
    };
    let expected = WRITERS * PER_WRITER + 1; // + the contended key
    assert_eq!(rows.len(), expected, "every distinct key is one row");
    assert_eq!(
        ellmephant::index::vec::count(&idle).expect("vectors") as usize,
        expected,
        "every row has a vector: a missing one is a dropped enqueue"
    );
    for (key, title, body, encode_hash) in rows {
        let want = ellmephant::store::row_hash(
            &spec,
            &ellmephant::embed::projection::embed_document(&key, &title, &body),
        );
        assert_eq!(
            encode_hash, want,
            "{key}: the index describes text the row no longer holds — a write was swallowed"
        );
    }
}

/// The whole path with the **real encoder**: weights loaded, Qwen3 running *on the worker's thread*,
/// real 1024-d vectors, and a real semantic comparison that puts the right row nearer the right
/// question.
///
/// `#[ignore]`d because it needs ~1.2 GB of weights — the rest of the suite runs on `HashEmbedder` so
/// it stays offline and fast. This is the one test that can catch a `Send`/re-entrancy problem in
/// driving the encoder from the worker's thread instead of a request's, which no fake can, and the
/// only one that can say the vectors are *meaningful* rather than merely present.
///
/// ```sh
/// ELLMEPHANT_TEST_MODEL_DIR=~/.ellmephant/models/Qwen--Qwen3-Embedding-0.6B/<sha> \
///   cargo test --locked --test encode_worker -- --ignored
/// ```
#[test]
#[ignore = "needs downloaded Qwen3 weights (ELLMEPHANT_TEST_MODEL_DIR)"]
fn the_worker_drives_the_real_encoder() {
    use ellmephant::embed::qwen3::{Options, Qwen3Embedder};

    let dir = std::env::var("ELLMEPHANT_TEST_MODEL_DIR")
        .expect("set ELLMEPHANT_TEST_MODEL_DIR to a flat weights dir");
    let (_tmp, paths) = common::temp_store();
    // The real model, so the config and the encoder agree by construction.
    let cfg = Config::default();
    let spec = Spec::from_config(&cfg, &paths);
    let embedder = Qwen3Embedder::load(
        std::path::Path::new(&dir),
        &Options::new(cfg.dim, "cpu", "bf16", ""),
    )
    .expect("load the real encoder");
    let session = Session::open(paths.clone(), cfg.clone(), spec)
        .expect("open store")
        .with_embedder(Box::new(embedder));
    let shared = Arc::new(Mutex::new(session));
    let server = MemoryServer::with_watch(Arc::clone(&shared), Arc::new(LockWatch::new()));
    let mut worker = server.spawn_encode_worker(Duration::from_millis(20));

    // Both writes return immediately. The WORKER pays the weight load and does the embedding — which is
    // the part no fake exercises.
    for (key, body) in [
        (
            "where-logs-live",
            "container logs ship to the ns1 log collector",
        ),
        (
            "finance-wash-sale",
            "a wash sale window reopens thirty days after the loss",
        ),
    ] {
        shared
            .lock()
            .unwrap()
            .memory_merge(&patch(key, body))
            .expect("write");
    }

    let idle = db::open_read_only(&paths).expect("read the store");
    let deadline = Instant::now() + Duration::from_secs(900);
    while ellmephant::index::vec::count(&idle).expect("vectors") < 2 {
        assert!(
            Instant::now() < deadline,
            "the worker never indexed both rows"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(ellmephant::store::encode::pending(&idle).expect("queue"), 0);
    worker.stop();

    // Real vectors, at the configured width — and read back through the index the worker wrote.
    let mut vectors = std::collections::HashMap::new();
    {
        let mut stmt = idle
            .prepare("SELECT key, id FROM memory ORDER BY key")
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .expect("query");
        for row in rows {
            let (key, id) = row.expect("row");
            let vector = ellmephant::index::vec::get(&idle, id)
                .expect("read the vector")
                .expect("a vector, since the worker indexed this row");
            assert_eq!(vector.len(), cfg.dim, "{key} is indexed at the width");
            vectors.insert(key, vector);
        }
    }

    // The one thing only the real encoder can answer: does it put the right row nearer the right
    // question? A fake cannot, and neither can a dimension check.
    let encoder = shared.lock().unwrap().encoder();
    let query = encoder
        .with(|e| e.embed_query("where do the logs go"))
        .expect("embed the query");
    let logs = dot(&query, &vectors["where-logs-live"]);
    let tax = dot(&query, &vectors["finance-wash-sale"]);
    assert!(
        logs > tax,
        "the logs memory ({logs:.4}) should beat the tax memory ({tax:.4}) for a logs question"
    );
}

/// Cosine similarity of two L2-normalised vectors: just the dot product.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// `stop()` returns, and the worker really has stopped: work queued afterwards is left alone rather
/// than picked up by a thread that is still running.
#[test]
fn stop_joins_the_worker_and_no_further_work_is_picked_up() {
    let (_dir, paths) = common::temp_store();
    let cfg = Config {
        model: MODEL.to_string(),
        dim: DIM,
        ..Config::default()
    };
    let spec = Spec::from_config(&cfg, &paths);
    let session = Session::open(paths.clone(), cfg, spec)
        .expect("open store")
        .with_embedder(Box::new(ellmephant::embed::HashEmbedder::new(DIM)));
    let shared = Arc::new(Mutex::new(session));
    let server = MemoryServer::with_watch(Arc::clone(&shared), Arc::new(LockWatch::new()));
    let mut worker = server.spawn_encode_worker(Duration::from_millis(5));

    // It works while running.
    shared
        .lock()
        .unwrap()
        .memory_merge(&patch("a", "alpha"))
        .expect("write");
    let idle = db::open_read_only(&paths).expect("read the store");
    let deadline = Instant::now() + Duration::from_secs(5);
    while ellmephant::index::vec::count(&idle).expect("vectors") == 0 {
        assert!(Instant::now() < deadline, "the worker never ran");
        std::thread::sleep(Duration::from_millis(10));
    }

    // Then it stops — promptly, since it is only ever between passes or inside a fast fake embed.
    let started = Instant::now();
    worker.stop();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stop() took {:?} to join: the worker is not honoring the stop flag between passes",
        started.elapsed()
    );

    // And work queued after the stop is stranded, which is the point: nothing is consuming it.
    shared
        .lock()
        .unwrap()
        .memory_merge(&patch("b", "beta"))
        .expect("write");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        ellmephant::store::encode::pending(&idle).expect("queue"),
        1,
        "the queue should be stranded with the worker stopped"
    );
}

/// Deleting a memory takes its queue entry with it, and so does retiring one into another.
///
/// A ghost entry is not corruption — the worker drops an entry whose row has gone — but it is a leak
/// nothing else would notice, and until it is collected a deleted key can still cause an embed.
#[test]
fn a_deleted_key_leaves_no_queue_entry() {
    let (_dir, paths) = common::temp_store();
    let conn = db::open(&paths).expect("open store");
    let spec = ellmephant::embed::VecSpec::new(MODEL, DIM);
    let queued = |conn: &rusqlite::Connection| -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT keyslug FROM encode_queue ORDER BY keyslug")
            .expect("prepare");
        let rows = stmt.query_map([], |r| r.get(0)).expect("query");
        rows.collect::<rusqlite::Result<Vec<String>>>()
            .expect("keyslugs")
    };

    ellmephant::store::add(&conn, &spec, &memory("gone", "never to be embedded")).expect("write");
    ellmephant::store::add(&conn, &spec, &memory("kept", "still owed a vector")).expect("write");
    assert_eq!(queued(&conn), ["memory:gone", "memory:kept"]);

    // Delete: the entry goes in the same transaction as the row.
    assert!(ellmephant::store::delete(&conn, "gone").expect("delete"));
    assert_eq!(
        queued(&conn),
        ["memory:kept"],
        "the deleted key's entry went with it"
    );

    // Retire one key into another. `new` already carries `old`'s prose, so nothing is integrated and
    // the only queue movement is the deleted key's entry going with its row.
    ellmephant::store::add(&conn, &spec, &memory("old", "deleted")).expect("write");
    ellmephant::store::add(
        &conn,
        &spec,
        &memory("new", "survivor, now carrying: deleted"),
    )
    .expect("write");
    assert_eq!(queued(&conn), ["memory:kept", "memory:new", "memory:old"]);
    ellmephant::store::links::merge_into(&conn, "old", "new").expect("merge");
    assert_eq!(
        queued(&conn),
        ["memory:kept", "memory:new"],
        "the deleted key's entry went with it"
    );
}
