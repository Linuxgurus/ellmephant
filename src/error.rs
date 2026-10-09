//! Error type shared across the crate.

use std::path::PathBuf;

/// Everything that can go wrong in llmphant.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("no such memory: {0}")]
    NotFound(String),

    /// Weights cache problem (missing/partial/corrupt; SPEC §6).
    #[error("model error: {0}")]
    Model(String),

    /// Transport failure from the fetch client (SPEC §6.2).
    #[error("http error: {0}")]
    Http(String),

    #[error("home directory not found (set HOME or LLMPHANT_HOME)")]
    NoHome,

    #[error("path must be valid utf-8: {}", .0.display())]
    NonUtf8Path(PathBuf),

    #[error("{0} is not implemented yet (see PLAN.md)")]
    NotImplemented(&'static str),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;
