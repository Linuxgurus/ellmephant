//! The worker as `serve()` actually starts it: a real binary, a real store, the real HTTP surface.
//!
//! Every other worker test builds a `MemoryServer` by hand and calls `spawn_encode_worker` itself —
//! which leaves the one thing that matters in production untested: **that `serve` starts one at all.**
//! This runs the binary the way the container does, writes over HTTP, and waits for the row to become
//! searchable with nobody's help. If `serve` forgot to spawn the worker, the row would sit in the queue
//! forever and this is the only test that would say so.
//!
//! `#[ignore]`d: it needs weights, because the encoder is the only thing the real binary can embed with.

#![cfg(feature = "encoder")]

mod common;

use std::time::{Duration, Instant};

use common::{Server, assert_port_free, request, wait_for_health};

/// A port of our own: the live store is on 8888, and this must not touch it. `ELLMEPHANT_TEST_PORT`
/// overrides, so a busy machine can move it.
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

    // The port has to be OURS: something already listening is a leaked server, and its store lives in
    // a temp dir that no longer exists.
    assert_port_free(port());
    // Killed on every exit path from here, a panic included.
    let _server = Server::start(home.path(), &weights, port());
    wait_for_health(port(), Duration::from_secs(60));

    // A write, over the real door. Nothing in this test drains anything by hand.
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

    // The worker loads the weights, embeds, and the row becomes findable — all with no help from us.
    // This is the assertion `serve` earns: if it never spawned a worker, this never passes.
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
