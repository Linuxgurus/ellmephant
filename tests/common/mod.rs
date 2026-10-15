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
