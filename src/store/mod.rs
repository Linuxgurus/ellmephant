//! The write/read path over `memory` and its derived indexes (P4).

pub mod links;
pub mod slug;
pub mod tags;

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// `memory.type` — how a memory is meant to be used (SPEC §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// How to find or derive something.
    Signpost,
    /// A rule to act on.
    Dictate,
    /// A plain fact.
    #[default]
    Fact,
    /// A sparse pointer used for routing only.
    Profile,
}

impl MemoryType {
    /// The string stored in the `type` column.
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryType::Signpost => "signpost",
            MemoryType::Dictate => "dictate",
            MemoryType::Fact => "fact",
            MemoryType::Profile => "profile",
        }
    }
}

/// One atomic memory: the storage *and* embedding unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    /// Rowid; `None` before insert and omitted from export (local/derived).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<i64>,
    /// Global-unique slug; identity and link target.
    pub key: String,
    /// Single bucket (`infra`, `projects`, ...).
    pub scope: String,
    /// Usage class.
    #[serde(rename = "type", default)]
    pub type_: MemoryType,
    /// Salient one-liner.
    pub title: String,
    /// Full, verifiable body.
    pub body: String,
    /// Provenance: command/path/repo/quote.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<String>,
    /// `sha256(model|dim|embed_text)`; re-embed only when this changes.
    #[serde(default)]
    pub hash: String,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Creation timestamp (SQLite `datetime('now')`).
    #[serde(default)]
    pub created: String,
    /// Last-update timestamp.
    #[serde(default)]
    pub updated: String,
}

/// Upsert a memory by `key`, keeping `memory`, tags, links and vectors in one
/// transaction. Implemented in P4.
pub fn add(_conn: &rusqlite::Connection, _mem: &Memory) -> Result<i64> {
    Err(crate::error::Error::NotImplemented(
        "store::add (PLAN.md P4)",
    ))
}
