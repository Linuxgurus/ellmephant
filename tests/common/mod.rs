//! Shared integration-test helpers.
//!
//! `#![allow(dead_code)]` because each test binary compiles this module separately and
//! uses only part of it.

#![allow(dead_code)]

use ellmephant::config::Paths;
use ellmephant::embed::{Embedder, VecSpec};
use ellmephant::store::{self, Memory};
use rusqlite::Connection;
use tempfile::TempDir;

/// A throwaway store root plus its resolved [`Paths`].
pub fn temp_store() -> (TempDir, Paths) {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = Paths::from_root(dir.path());
    (dir, paths)
}

/// Shorthand for a one-row `SELECT count(...)`.
pub fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count query")
}

/// Settle every pending document.
///
/// A write no longer embeds — it upserts the row and *queues* the document, and the worker (absent in
/// a test) is what turns the queue into vectors. So a test that cares about search results, duplicate
/// detection or `stats` must settle first, and this is the one call that says so. `ENCODE-WORKER.md`
/// is the design; this is the test-side door.
pub fn settle(conn: &Connection, embedder: &dyn Embedder) {
    let spec = VecSpec::of(embedder);
    while store::encode::drain(conn, embedder, &spec, 64).expect("drain") > 0 {}
}

/// Write a memory and settle it: the synchronous shape a test wants, and the honest one for a
/// single write. Returns what [`store::add`] returns, so `.unwrap()`/`.expect(..)` at the call site
/// still reads the same.
pub fn add_now(
    conn: &Connection,
    embedder: &dyn Embedder,
    mem: &Memory,
) -> Result<(i64, String), ellmephant::error::Error> {
    let spec = VecSpec::of(embedder);
    let out = store::add(conn, &spec, mem)?;
    settle(conn, embedder);
    Ok(out)
}

/// [`store::merge`] and settle. Returns the outcome, so a test can still assert on `queued`.
pub fn merge_now(
    conn: &Connection,
    embedder: &dyn Embedder,
    patch: &store::Patch,
) -> Result<store::MergeOutcome, ellmephant::error::Error> {
    let spec = VecSpec::of(embedder);
    let out = store::merge(conn, &spec, patch)?;
    settle(conn, embedder);
    Ok(out)
}

// --- The out-of-process harness ---------------------------------------------------------------
//
// Two tests need to run the REAL binary over HTTP (`serve_wiring`, `reembed_no_outage`). This is the
// one copy of that machinery, so they cannot drift apart.

/// A running `ellmephant mcp`, killed on the way out **whatever happens — a panic included**.
///
/// A leaked server holds the port, and the next run then talks to a zombie whose store was deleted
/// along with its temp dir. That is not hypothetical: it is how `serve_wiring` failed the first time it
/// was written, and it cost ten minutes of a hung search loop to find out.
pub struct Server(std::process::Child);

impl Server {
    /// Spawn `ellmephant mcp` over `home`, embedding with the flat weights at `weights`
    /// (`--model-dir` implies offline, so a test never fetches).
    pub fn start(home: &std::path::Path, weights: &str, port: u16) -> Server {
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_ellmephant"))
            .args(["--home", home.to_str().expect("utf8 path")])
            .args(["--model-dir", weights])
            .args(["mcp", "--host", "127.0.0.1", "--port"])
            .arg(port.to_string())
            .env("ELLMEPHANT_WRITE_TOKEN", TOKEN)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn `ellmephant mcp`");
        Server(child)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The bearer every spawn gets. Reads do not need it; writes do.
pub const TOKEN: &str = "tok";

/// Fail loudly if something is already listening: it would be a leaked server, and its store lives in
/// a temp dir that no longer exists.
pub fn assert_port_free(port: u16) {
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "port {port} is in use (a leaked server?); free it with \
         `pkill -f 'target/debug/ellmephant --home'` and retry"
    );
}

/// One hand-rolled request, returning the raw response — headers, chunk framing and all.
///
/// No HTTP client on purpose: these tests care about the *wording* of a reply, and scanning raw bytes
/// for a token is immune to chunked transfer-encoding, which a parsed body would not be.
pub fn request(port: u16, method: &str, path: &str, body: Option<&str>) -> Option<String> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .ok()?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
         Authorization: Bearer {TOKEN}\r\n"
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

/// Block until `/health` answers from a server we started, or fail.
pub fn wait_for_health(port: u16, within: std::time::Duration) {
    let deadline = std::time::Instant::now() + within;
    loop {
        // The body must be OURS: a stranger on the port would answer something else.
        if request(port, "GET", "/health", None).is_some_and(|h| h.contains("ellmephant")) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "`ellmephant mcp` never answered /health on port {port}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Count a scalar out of the store file, read-only, with the server running.
pub fn scalar(db: &std::path::Path, sql: &str) -> i64 {
    ellmephant::index::vec::register_extension();
    let conn = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open read-only");
    conn.query_row(sql, [], |r| r.get(0)).expect("scalar")
}
