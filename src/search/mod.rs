//! Hybrid search orchestration: run both legs, fuse with RRF, hydrate rows (P5).

pub mod rrf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Which retrieval legs to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// BM25 ∪ KNN, fused with RRF.
    #[default]
    Hybrid,
    /// KNN only.
    Vec,
    /// BM25 only.
    Fts,
}

impl Mode {
    /// Parse a CLI/config string (`hybrid`, `vec`, `fts`).
    pub fn parse(s: &str) -> Result<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "hybrid" => Ok(Mode::Hybrid),
            "vec" => Ok(Mode::Vec),
            "fts" => Ok(Mode::Fts),
            other => Err(crate::error::Error::Config(format!(
                "invalid mode {other:?} (want hybrid|vec|fts)"
            ))),
        }
    }

    /// Canonical name.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Hybrid => "hybrid",
            Mode::Vec => "vec",
            Mode::Fts => "fts",
        }
    }
}

/// Search request.
#[derive(Debug, Clone)]
pub struct Query {
    /// The user's question.
    pub text: String,
    /// Restrict to one bucket.
    pub scope: Option<String>,
    /// Result count.
    pub k: usize,
    /// `hybrid` (default), `vec` or `fts`.
    pub mode: Mode,
    /// Optional cosine-distance ceiling.
    pub max_distance: Option<f32>,
    /// Link-neighbourhood hops to include (0 = off).
    pub expand: u8,
}

/// One hydrated hit. Scores are returned so the caller can judge confidence
/// (SPEC §14 false-memory hazard).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    /// `memory.id`.
    pub id: i64,
    /// Slug.
    pub key: String,
    /// Bucket.
    pub scope: String,
    /// Usage class.
    #[serde(rename = "type")]
    pub type_: String,
    /// Salient line.
    pub title: String,
    /// Full body (already bounded by the caller's snippet policy).
    pub body: String,
    /// Fused score.
    pub score: f64,
    /// Vector distance when the KNN leg contributed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
    /// Attached tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Run a hybrid query. Implemented in P5.
pub fn run(_conn: &rusqlite::Connection, _q: &Query) -> Result<Vec<Hit>> {
    Err(Error::NotImplemented("search::run (PLAN.md P5)"))
}
