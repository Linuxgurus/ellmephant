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
    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("{0}")]
    Invalid(String),

    #[error("{0}")]
    TooLarge(String),

    #[error("index unavailable: {0}")]
    Unavailable(String),

    #[error("no such project: {0}")]
    NoProject(String),

    #[error("project error: {0}")]
    Project(String),

    #[error("claim error: {0}")]
    Claim(String),

    #[error("invalid pattern: {0}")]
    Pattern(String),

    #[error("model error: {0}")]
    Model(String),

    #[error("http error: {0}")]
    Http(String),

    #[error("home directory not found (set HOME or ELLMEPHANT_HOME)")]
    NoHome,

    #[error("{0}")]
    Unsupported(&'static str),

    #[error("internal error: {0}")]
    Internal(String),
}

impl From<Error> for String {
    fn from(error: Error) -> Self {
        error.to_string()
    }
}

pub type Result<T> = std::result::Result<T, Error>;
