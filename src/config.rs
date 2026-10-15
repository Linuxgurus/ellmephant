//! Paths and configuration: `~/.ellmephant/`, `config.toml`, and the merge of
//! defaults <- file <- flags.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Directory name under `$HOME` unless `ELLMEPHANT_HOME` overrides it.
pub const APP_DIR: &str = ".ellmephant";
/// The single source of truth.
pub const DB_FILE: &str = "memory.db";
/// Optional user config.
pub const CONFIG_FILE: &str = "config.toml";
/// Weight cache (or `HF_HOME`).
pub const MODELS_DIR: &str = "models";
/// REPL history.
pub const HISTORY_FILE: &str = "history";

/// The default instruction prefix applied to the *query* side only (SPEC §6).
pub const DEFAULT_INSTRUCTION: &str = "Given a question, retrieve relevant memories";

/// Default TCP port for the MCP server (SPEC §11). High and unassigned.
pub const DEFAULT_PORT: u16 = 8888;

/// Accepted `device` values — the one list `Config::validate` and the encoder both consult.
pub const DEVICES: [&str; 2] = ["cpu", "metal"];
/// Accepted `precision` values — likewise shared with the encoder.
pub const PRECISIONS: [&str; 3] = ["bf16", "f16", "f32"];
/// Largest embedding dimension MRL allows (SPEC §6); the frozen `vec0` width.
pub const MAX_DIM: usize = 1024;

/// Default cosine-distance ceiling for the KNN leg when a search omits `max_distance`
/// (SPEC §10). Calibrated on the sample store: the rank-1 hit for a relevant query sits
/// ≲0.6 while the noise floor is ≳0.63, so 0.6 keeps every genuine hit (even a sloppy
/// paraphrase) and still drops the chaff. High-precision callers can tighten toward 0.5.
pub const DEFAULT_MAX_DISTANCE: f32 = 0.6;

/// Resolved on-disk locations. `ELLMEPHANT_HOME` wins over `$HOME/.ellmephant`, which
/// keeps tests hermetic without touching the real store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// Discover the store root from the environment.
    pub fn discover() -> Result<Self> {
        if let Some(home) = std::env::var_os("ELLMEPHANT_HOME") {
            return Ok(Self {
                root: PathBuf::from(home),
            });
        }
        let home = std::env::var_os("HOME").ok_or(Error::NoHome)?;
        Ok(Self {
            root: PathBuf::from(home).join(APP_DIR),
        })
    }

    /// Root an explicit store directory (used by tests and `--home`).
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Store root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `memory.db`.
    pub fn db(&self) -> PathBuf {
        self.root.join(DB_FILE)
    }

    /// `config.toml`.
    pub fn config(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
    }

    /// `models/`.
    pub fn models(&self) -> PathBuf {
        self.root.join(MODELS_DIR)
    }

    /// `history`.
    pub fn history(&self) -> PathBuf {
        self.root.join(HISTORY_FILE)
    }

    /// Create the store root (and `models/`) if missing.
    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(self.models())?;
        Ok(())
    }
}

/// User-tunable settings. Flags override these; these override the defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Embedding model id (HF repo or local dir name).
    pub model: String,
    /// Revision to load: a branch/tag (resolved once to a sha, then pinned) or a sha.
    pub revision: String,
    /// Weights cache root; empty means `<home>/models` (SPEC §6.1).
    pub cache_dir: String,
    /// Never open a socket; a cold cache is a hard error (SPEC §6.4).
    pub offline: bool,
    /// Vector dimensionality; frozen at `CREATE`. MRL allows 32..=1024 and the cap is enforced.
    pub dim: usize,
    /// `bf16` (default), `f16` or `f32` (CPU forces `f32`).
    pub precision: String,
    /// `cpu` or `metal`.
    pub device: String,
    /// Query-side instruction string, mirrored into `meta`.
    pub instruction: String,
    /// Default number of results for search.
    pub default_k: usize,
    /// Cosine-distance ceiling applied when a search omits `max_distance` (SPEC §10).
    /// `null` disables the cap.
    pub default_max_distance: Option<f32>,
    /// TCP port for the MCP server (SPEC §11); `ellmephant mcp` listens here by default.
    pub port: u16,
    /// How long the encode worker sleeps between jobs, in milliseconds (`ENCODE-WORKER.md`). This is
    /// also its idle poll interval, so it bounds how long a freshly written memory stays invisible to
    /// a *semantic* search — one number, because a separate "between jobs" and "idle" interval would
    /// be a knob nobody can set correctly.
    pub encode_interval_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "Qwen/Qwen3-Embedding-0.6B".to_string(),
            revision: "main".to_string(),
            cache_dir: String::new(),
            offline: false,
            dim: 1024,
            precision: "bf16".to_string(),
            device: "cpu".to_string(),
            instruction: DEFAULT_INSTRUCTION.to_string(),
            default_k: 5,
            default_max_distance: Some(DEFAULT_MAX_DISTANCE),
            port: DEFAULT_PORT,
            encode_interval_ms: 1_000,
        }
    }
}

impl Config {
    /// Load `config.toml` if present, else defaults, and reject an unusable config up front.
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config();
        let cfg = if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            toml::from_str(&text)?
        } else {
            Self::default()
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject a bad `device`/`precision`/`dim` before anything opens a model, so the error
    /// is one clear message instead of surfacing at encoder load.
    pub fn validate(&self) -> Result<()> {
        if !DEVICES.contains(&self.device.as_str()) {
            return Err(Error::Config(format!(
                "invalid device {:?} (want {})",
                self.device,
                DEVICES.join("|")
            )));
        }
        if !PRECISIONS.contains(&self.precision.as_str()) {
            return Err(Error::Config(format!(
                "invalid precision {:?} (want {})",
                self.precision,
                PRECISIONS.join("|")
            )));
        }
        if self.dim == 0 || self.dim > MAX_DIM {
            return Err(Error::Config(format!(
                "dim must be 1..={MAX_DIM} (got {})",
                self.dim
            )));
        }
        Ok(())
    }
}
