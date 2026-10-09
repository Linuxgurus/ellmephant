//! Read JSONL back into the store (P8).

use std::path::Path;

use crate::error::{Error, Result};

/// Upsert by `key`; re-embed only on hash change. `dry_run` reports the diff.
pub fn from_jsonl(_conn: &rusqlite::Connection, _path: &Path, _dry_run: bool) -> Result<usize> {
    Err(Error::NotImplemented("import::from_jsonl (PLAN.md P8)"))
}
