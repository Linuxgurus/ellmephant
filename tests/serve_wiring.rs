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

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// A port of our own: the live store is on 8888, and this must not touch it. `ELLMEPHANT_TEST_PORT`
/// overrides, so a busy machine can move it.
fn port() -> u16 {
    std::env::var("ELLMEPHANT_TEST_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8973)
}

/// The server process, killed on the way out **whatever happens — including a panic**.
///
/// A test that leaked a server would hold the port, and the next run would then talk to a zombie whose
/// store had been deleted along with its temp dir. That is precisely how this test failed the first
/// time it was written, and it cost ten minutes of a hung search loop to find out.
struct Server(std::process::Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One hand-rolled request. No HTTP client on purpose: what this test cares about is the *wording* of
/// the replies, and scanning the raw bytes for a token is immune to chunked transfer-encoding, which a
/// parsed body would not be.
fn request(method: &str, path: &str, body: Option<&str>) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port())).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
         Authorization: Bearer tok\r\n"
    );
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    if let Some(body) = body {
        head.push_str(body);
    }
    stream.write_all(head.as_bytes()).ok()?;
    let mut out = String::new();
    stream.read_to_string(&mut out).ok()?;
    Some(out)
}

#[test]
#[ignore = "needs Qwen3 weights (ELLMEPHANT_TEST_MODEL_DIR)"]
fn serve_starts_a_worker_that_indexes_what_was_written() {
    let weights =
        std::env::var("ELLMEPHANT_TEST_MODEL_DIR").expect("set ELLMEPHANT_TEST_MODEL_DIR");
    let home = tempfile::tempdir().expect("tempdir");

    // The port has to be OURS. Something already listening means a leaked server from an earlier run,
    // and talking to it would be nonsense — its store lives in a temp dir that no longer exists.
    assert!(
        TcpStream::connect(("127.0.0.1", port())).is_err(),
        "port {} is already in use (a leaked server?); free it with \
         `pkill -f 'target/debug/ellmephant --home'` and retry",
        port()
    );

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_ellmephant"))
        .args(["--home", home.path().to_str().expect("utf8 path")])
        // `--model-dir` points the encoder at the flat weights and implies offline.
        .args(["--model-dir", &weights])
        .args(["mcp", "--host", "127.0.0.1", "--port"])
        .arg(port().to_string())
        .env("ELLMEPHANT_WRITE_TOKEN", "tok")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn `ellmephant mcp`");
    // Killed on every exit path from here, panic included.
    let _server = Server(child);

    // Wait for the surface, and check it is OURS: a busy port would answer with somebody else's bytes.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match request("GET", "/health", None) {
            Some(health) if health.contains("ellmephant") => break,
            _ => {
                assert!(
                    Instant::now() < deadline,
                    "`ellmephant mcp` never answered /health on port {}",
                    port()
                );
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    // A write, over the real door. Nothing in this test drains anything by hand.
    let reply = request(
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
            request("GET", "/health?format=json", None).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}
