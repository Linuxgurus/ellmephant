//! Shared integration-test helpers.
//!
//! `#![allow(dead_code)]` because each test binary compiles this module separately and
//! uses only part of it.

#![allow(dead_code)]

use llmphant::config::Paths;
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
