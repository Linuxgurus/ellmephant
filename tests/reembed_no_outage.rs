//! The incident, as an acceptance test: re-embedding a REAL store must not stop the store.
//!
//! On 2026-10-15 one `POST /reembed` held the single writer for ~45 minutes on a 291-row store, and
//! every write during it timed out at 60s while `/health` cheerfully reported `ready: true`. This runs
//! the real binary against a **copy** of that store, queues a full rebuild, and asserts the things that
//! were false then:
//!
//!   1. the reembed returns in seconds, having queued the whole store;
//!   2. a write lands while the rebuild is still draining;
//!   3. the worker is really indexing real rows, at scale, on real vectors.
//!
//! The trigger is honest: the projection fingerprint is poked, which is exactly what a code change to
//! `embed_document` looks like from the store's side — and it is why this store needs a re-embed at all.
//!
//! `#[ignore]`d: it needs weights AND a real store, and it does real embeddings.
//!
//! ```sh
//! ELLMEPHANT_TEST_MODEL_DIR=<flat weights> \
//! ELLMEPHANT_TEST_BASELINE_DB=~/.tmp/hygiene/baseline.db \
//!   cargo test --locked --test reembed_no_outage -- --ignored
//! ```

#![cfg(feature = "encoder")]

mod common;

use std::time::{Duration, Instant};

use common::{Server, assert_port_free, request, scalar, wait_for_health};

/// Our own port: the live store is 8888, and the other out-of-process test takes 8973.
fn port() -> u16 {
    std::env::var("ELLMEPHANT_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8975)
}

/// How long we let the worker drain before judging progress.
///
/// **Bounded on purpose.** A real re-embed of ~280 rows is minutes of CPU, and the offline soak plus the
/// unit tests already assert *full* convergence; paying that here would buy nothing and make a test
/// nobody keeps. What this one measures is the thing that broke: that a rebuild does not hold the store,
/// at scale, on real data. The printed rate is the number worth having.
const DRAIN_BUDGET: Duration = Duration::from_secs(150);

/// The floor that makes "the worker is consuming the queue" a claim rather than a hope.
///
/// Low on purpose. The measured rate on a debug laptop build is ~11.5s per REAL row (13 rows in the
/// 150s below), because a memory body is hundreds of tokens and this is an unoptimised candle: a full
/// 281-row re-embed is the better part of an hour. That is not the thing this test is about — the
/// reembed CALL returns instantly and the store stays writable throughout, which is what the incident
/// was. What has to hold here is that the queue is being consumed at all.
const MIN_DRAINED: i64 = 5;

/// The JSON body of a response, whether or not it was chunked.
fn json(reply: &str) -> serde_json::Value {
    let body = reply
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or(reply);
    serde_json::from_str(body.trim_start()).expect("a JSON reply")
}

#[test]
#[ignore = "needs weights and a real store (ELLMEPHANT_TEST_MODEL_DIR, ELLMEPHANT_TEST_BASELINE_DB)"]
fn reembedding_a_real_store_does_not_stop_it() {
    let weights =
        std::env::var("ELLMEPHANT_TEST_MODEL_DIR").expect("set ELLMEPHANT_TEST_MODEL_DIR");
    let baseline = std::env::var("ELLMEPHANT_TEST_BASELINE_DB").unwrap_or_else(|_| {
        format!(
            "{}/.tmp/hygiene/baseline.db",
            std::env::var("HOME").expect("HOME")
        )
    });
    assert_port_free(port());

    // A COPY, never the original: this test writes, pokes `meta`, and is allowed to be wrong.
    let home = tempfile::tempdir().expect("tempdir");
    let db = home.path().join("memory.db");
    std::fs::copy(&baseline, &db).expect("copy the baseline store");
    // `encode_interval_ms = 0` so the worker drains flat out. The 1s default is right for a live store
    // and wrong for a test that wants to watch the queue move.
    std::fs::write(home.path().join("config.toml"), "encode_interval_ms = 0\n").expect("config");

    // Poke the projection: a code change to the projection, seen from the store's side.
    {
        let conn = rusqlite::Connection::open(&db).expect("open the copy");
        conn.execute(
            "UPDATE meta SET v = 'moved-by-the-test' WHERE k = 'projection'",
            [],
        )
        .expect("poke the projection");
    }
    let rows = scalar(&db, "SELECT count(*) FROM memory");
    assert!(rows > 200, "this test is about scale; got {rows} rows");

    let _server = Server::start(home.path(), &weights, port());
    wait_for_health(port(), Duration::from_secs(60));

    // 1. The rebuild RETURNS, having queued the whole store. This is what used to take 45 minutes.
    let started = Instant::now();
    let reply = request(port(), "POST", "/reembed?format=json", None).expect("POST /reembed");
    let took = started.elapsed();
    let body = json(&reply);
    assert_eq!(body["status"], "queued", "{reply}");
    assert_eq!(
        body["queued"].as_u64(),
        Some(rows as u64),
        "every row should be owed a vector: {reply}"
    );
    assert!(
        took < Duration::from_secs(30),
        "the reembed took {took:?}: it is holding the store again instead of queueing"
    );

    // 2. A write lands while that queue is still draining — the outage, asserted away.
    let started = Instant::now();
    let wrote = request(
        port(),
        "POST",
        "/memory?format=json",
        Some(
            r#"{"key":"during-reembed","title":"During reembed","body":"this write must not wait for the rebuild"}"#,
        ),
    )
    .expect("POST /memory during the rebuild");
    let waited = started.elapsed();
    assert!(json(&wrote)["queued"].as_bool().unwrap_or(false), "{wrote}");
    assert!(
        waited < Duration::from_secs(10),
        "a write waited {waited:?} behind the rebuild"
    );

    // 3. And the worker is consuming that queue — measured on the QUEUE, never on the vector count:
    //    after a projection change the old vectors are still present (stale, describing the old
    //    recipe), so counting them would prove nothing and pass instantly. Which it did, before this
    //    comment was written.
    //
    //    The reply's number is the authoritative "what was queued"; the live queue is then expected to
    //    be *smaller*, because a worker at interval 0 starts consuming immediately.
    let pending = |db: &std::path::Path| scalar(db, "SELECT count(*) FROM encode_queue");
    let queued = body["queued"].as_u64().expect("queued") as i64;
    assert_eq!(queued, rows, "every row should be owed a vector");

    // The write in (2) queued one more document, so `queued + 1` is the most that can be outstanding.
    let outstanding = queued + 1;
    let mut left = pending(&db);
    assert!(
        left <= outstanding,
        "the queue grew past the rebuild plus our own write: {left} > {outstanding}"
    );
    let deadline = Instant::now() + DRAIN_BUDGET;
    while left > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        left = pending(&db);
    }
    let drained = outstanding - left;
    eprintln!("drained {drained} of {outstanding} real rows within {DRAIN_BUDGET:?} ({left} left)");
    assert!(
        drained >= MIN_DRAINED,
        "only {drained} rows were drained in {DRAIN_BUDGET:?}: the worker is not consuming the queue"
    );
}
