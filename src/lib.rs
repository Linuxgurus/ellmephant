//! ellmephant — single-binary, SQLite-backed long-term memory for AI agents.
//!
//! One SQLite file is the source of truth; vector, full-text and link indexes are
//! derived and rebuildable. See `SPEC.md` for the decision record and `PLAN.md` for
//! the phased build.

pub mod bench;
pub mod cli;
pub mod config;
pub mod db;
pub mod embed;
pub mod error;
pub mod hex;
pub mod index;
pub mod mcp;
pub mod model;
pub mod search;
pub mod store;
pub mod wire;

pub use config::{Config, Paths};
pub use error::{Error, Result};

/// Semantic version of the crate, reported by `ellmephant --version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
