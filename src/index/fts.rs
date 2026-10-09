//! FTS5 / BM25 leg (P5).
//!
//! BM25 is not optional: it carries exact tokens (hostnames, paths, identifiers) that
//! embeddings fumble (SPEC §10).

use crate::error::{Error, Result};

/// A BM25 hit: rowid plus the (negative-is-better) bm25 score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FtsHit {
    /// `memory.id`.
    pub id: i64,
    /// `bm25(memory_fts)`; more negative is a better match.
    pub score: f64,
}

/// Rank `memory_fts` for `query`, optionally scoped.
pub fn search(
    _conn: &rusqlite::Connection,
    _query: &str,
    _scope: Option<&str>,
    _limit: usize,
) -> Result<Vec<FtsHit>> {
    Err(Error::NotImplemented("index::fts::search (PLAN.md P5)"))
}
