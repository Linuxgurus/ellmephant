//! Write the store out as JSONL (P8).

use std::path::Path;

use crate::error::{Error, Result};

/// Stream every `memory` row to `path` as one JSON object per line.
pub fn to_jsonl(_conn: &rusqlite::Connection, _path: &Path) -> Result<usize> {
    Err(Error::NotImplemented("export::to_jsonl (PLAN.md P8)"))
}
