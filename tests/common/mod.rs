#![allow(dead_code)]

use ellmephant::config::Paths;
use ellmephant::embed::{Embedder, VecSpec};
use ellmephant::store::{self, Memory};
use rusqlite::Connection;
use tempfile::TempDir;

pub fn temp_store() -> (TempDir, Paths) {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = Paths::from_root(dir.path());
    (dir, paths)
}

pub fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count query")
}

pub fn settle(conn: &Connection, embedder: &dyn Embedder) {
    let _spec = VecSpec::of(embedder);
    while store::encode::drain(conn, embedder, 64).expect("drain") > 0 {}
}

pub fn add_now(
    conn: &Connection,
    embedder: &dyn Embedder,
    mem: &Memory,
) -> Result<(i64, String), ellmephant::error::Error> {
    let _spec = VecSpec::of(embedder);
    let out = store::add(conn, mem)?;
    settle(conn, embedder);
    Ok(out)
}

pub fn merge_now(
    conn: &Connection,
    embedder: &dyn Embedder,
    patch: &store::Patch,
) -> Result<store::MergeOutcome, ellmephant::error::Error> {
    let _spec = VecSpec::of(embedder);
    let out = store::merge(conn, patch)?;
    settle(conn, embedder);
    Ok(out)
}

pub struct Server(std::process::Child);

impl Server {
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

pub const TOKEN: &str = "tok";

pub fn assert_port_free(port: u16) {
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "port {port} is in use (a leaked server?); free it with \
         `pkill -f 'target/debug/ellmephant --home'` and retry"
    );
}

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

pub fn wait_for_health(port: u16, within: std::time::Duration) {
    let deadline = std::time::Instant::now() + within;
    loop {
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

pub fn scalar(db: &std::path::Path, sql: &str) -> i64 {
    ellmephant::index::vec::register_extension();
    let conn = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open read-only");
    conn.query_row(sql, [], |r| r.get(0)).expect("scalar")
}
