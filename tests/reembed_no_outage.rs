#![cfg(feature = "encoder")]

mod common;

use std::time::{Duration, Instant};

use common::{Server, assert_port_free, request, scalar, wait_for_health};

fn port() -> u16 {
    std::env::var("ELLMEPHANT_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8975)
}

const DRAIN_BUDGET: Duration = Duration::from_secs(150);

const MIN_DRAINED: i64 = 5;

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

    let home = tempfile::tempdir().expect("tempdir");
    let db = home.path().join("memory.db");
    std::fs::copy(&baseline, &db).expect("copy the baseline store");
    std::fs::write(home.path().join("config.toml"), "encode_interval_ms = 0\n").expect("config");

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

    let pending = |db: &std::path::Path| scalar(db, "SELECT count(*) FROM encode_queue");
    let queued = body["queued"].as_u64().expect("queued") as i64;
    assert_eq!(queued, rows, "every row should be owed a vector");

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
