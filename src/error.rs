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
    /// The row EXISTS and the caller may not do this to it. Distinct from [`Error::NotFound`] on
    /// purpose (James, 2026-10-24): telling an agent *"no such memory"* when the memory is there and
    /// the caller simply lacks the reach is a lie that costs it a debugging session. Reads keep the
    /// absence rule — a read must not confirm a row it may not see — but a WRITE says plainly that
    /// the answer is no.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// A write the store refuses: a field that may not be empty, or a create missing its minimum.
    /// The message is written to be read by the caller — it says what to do instead.
    #[error("{0}")]
    Invalid(String),

    /// A write past a store limit — the body cap. Its own variant so a door can answer 413 rather
    /// than the generic 400: the request is well formed, it is simply too big.
    #[error("{0}")]
    TooLarge(String),

    /// A derived index the caller needs is not there (an empty vector table with rows to read). A
    /// read must say so rather than answer an empty result, because *no hits* and *no index* look
    /// identical to a caller and only one of them is true.
    #[error("index unavailable: {0}")]
    Unavailable(String),

    /// A project key that resolves to no row — the project half of [`Error::NotFound`].
    #[error("no such project: {0}")]
    NoProject(String),

    /// A project-layer rule was violated: an empty key, an unknown status, or a `prj:` marker where
    /// only memories belong.
    #[error("project error: {0}")]
    Project(String),

    /// A claim-layer rule was violated: an empty name/owner, or a TTL that is not a usable duration.
    #[error("claim error: {0}")]
    Claim(String),

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
