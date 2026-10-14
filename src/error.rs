//! Error type shared across the crate.

/// Everything that can go wrong in ellmephant.
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

    /// A project key that resolves to no row (the `prj-` counterpart of [`Error::NotFound`]).
    #[error("no such project: {0}")]
    NoProject(String),

    /// A project-layer rule was violated: a key without the `prj-` prefix, an unknown status or
    /// owner, or a second home for a memory that already belongs to a project.
    #[error("project error: {0}")]
    Project(String),

    /// A malformed search pattern (a regex that will not compile); the message carries the
    /// compiler's complaint.
    #[error("invalid pattern: {0}")]
    Pattern(String),

    /// Weights cache problem (missing/partial/corrupt; SPEC §6).
    #[error("model error: {0}")]
    Model(String),

    /// Transport failure from the fetch client (SPEC §6.2).
    #[error("http error: {0}")]
    Http(String),

    #[error("home directory not found (set HOME or ELLMEPHANT_HOME)")]
    NoHome,

    /// A capability this build does not carry (e.g. the encoder behind a disabled feature).
    #[error("{0}")]
    Unsupported(&'static str),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;
