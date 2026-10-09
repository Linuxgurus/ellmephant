//! Paths and configuration: `~/.llmphant/`, `config.toml`, and the merge of
//! defaults <- file <- flags.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Directory name under `$HOME` unless `LLMPHANT_HOME` overrides it.
pub const APP_DIR: &str = ".llmphant";
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

/// Resolved on-disk locations. `LLMPHANT_HOME` wins over `$HOME/.llmphant`, which
/// keeps tests hermetic without touching the real store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// Discover the store root from the environment.
    pub fn discover() -> Result<Self> {
        if let Some(home) = std::env::var_os("LLMPHANT_HOME") {
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
    /// Vector dimensionality; frozen at `CREATE`, MRL allows 32..=1024.
    pub dim: usize,
    /// `bf16` (default) or `f16`.
    pub precision: String,
    /// `cpu` or `metal`.
    pub device: String,
    /// Query-side instruction string, mirrored into `meta`.
    pub instruction: String,
    /// Default number of results for search.
    pub default_k: usize,
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
            default_k: 10,
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
    /// is one clear message instead of surfacing at encoder load (REFACTOR follow-up).
    pub fn validate(&self) -> Result<()> {
        const DEVICES: [&str; 2] = ["cpu", "metal"];
        const PRECISIONS: [&str; 3] = ["bf16", "f16", "f32"];
        if !DEVICES.contains(&self.device.as_str()) {
            return Err(Error::Config(format!(
                "invalid device {:?} (want cpu|metal)",
                self.device
            )));
        }
        if !PRECISIONS.contains(&self.precision.as_str()) {
            return Err(Error::Config(format!(
                "invalid precision {:?} (want bf16|f16|f32)",
                self.precision
            )));
        }
        if self.dim == 0 {
            return Err(Error::Config("dim must be positive".into()));
        }
        Ok(())
    }
}
