#![cfg(feature = "encoder")]

mod common;

use std::time::{Duration, Instant};

use common::{Server, assert_port_free, request, wait_for_health};

fn port() -> u16 {
    std::env::var("ELLMEPHANT_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8973)
}

#[test]
#[ignore = "needs Qwen3 weights (ELLMEPHANT_TEST_MODEL_DIR)"]
fn serve_starts_a_worker_that_indexes_what_was_written() {
    let weights =
        std::env::var("ELLMEPHANT_TEST_MODEL_DIR").expect("set ELLMEPHANT_TEST_MODEL_DIR");
    let home = tempfile::tempdir().expect("tempdir");

    assert_port_free(port());
    let _server = Server::start(home.path(), &weights, port());
    wait_for_health(port(), Duration::from_secs(60));

    let reply = request(port(),
        "POST",
        "/memory?format=json",
        Some(
            r#"{"key":"serve-wiring","title":"Serve wiring","body":"the worker must be started by serve, or this row never becomes searchable"}"#,
        ),
    )
    .expect("POST /memory");
    assert!(
        reply.contains("\"queued\":true"),
        "the write should have queued its document: {reply}"
    );

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let found = request(
            port(),
            "GET",
            "/search?q=which+worker+indexes+what+was+written&format=json",
            None,
        )
        .unwrap_or_default();
        if found.contains("serve-wiring") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the row never became searchable — did `serve` start the worker? {}",
            request(port(), "GET", "/health?format=json", None).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}
