//! Portability: JSONL export/import (P8).
//!
//! Canonical fields `{key,scope,type,title,body,source,tags,created,updated}` — no
//! ids, no vectors (both local/derived). Import is upsert-by-key and re-embeds only
//! when the row hash changes, so a second import is a no-op.

pub mod export;
pub mod import;

use serde::{Deserialize, Serialize};

use crate::store::{Memory, MemoryType};

/// The canonical JSONL record: the export/import unit (SPEC §11).
///
/// Deliberately narrower than [`Memory`]: `id`, `hash`, `model` and `dim` are
/// local/derived, so omitting them keeps a dump portable across machines, model
/// revisions and vector dimensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Global-unique slug.
    pub key: String,
    /// Single bucket.
    pub scope: String,
    /// Usage class.
    #[serde(rename = "type", default)]
    pub type_: MemoryType,
    /// Salient one-liner.
    pub title: String,
    /// Full, verifiable body.
    pub body: String,
    /// Provenance.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<String>,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Creation timestamp.
    #[serde(default)]
    pub created: String,
    /// Last-update timestamp.
    #[serde(default)]
    pub updated: String,
}

impl From<Memory> for Record {
    fn from(mem: Memory) -> Self {
        Self {
            key: mem.key,
            scope: mem.scope,
            type_: mem.type_,
            title: mem.title,
            body: mem.body,
            source: mem.source,
            tags: mem.tags,
            created: mem.created,
            updated: mem.updated,
        }
    }
}

impl From<Record> for Memory {
    fn from(rec: Record) -> Self {
        Memory {
            id: None,
            key: rec.key,
            scope: rec.scope,
            type_: rec.type_,
            title: rec.title,
            body: rec.body,
            source: rec.source,
            hash: String::new(),
            tags: rec.tags,
            created: rec.created,
            updated: rec.updated,
        }
    }
}
