use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const APP_DIR: &str = ".ellmephant";
pub const DB_FILE: &str = "memory.db";
pub const CONFIG_FILE: &str = "config.toml";
pub const MODELS_DIR: &str = "models";
pub const HISTORY_FILE: &str = "history";

pub const DEFAULT_INSTRUCTION: &str = "Given a question, retrieve relevant memories";

pub const DEFAULT_PORT: u16 = 8888;

pub const DEVICES: [&str; 2] = ["cpu", "metal"];
pub const PRECISIONS: [&str; 3] = ["bf16", "f16", "f32"];
pub const MAX_DIM: usize = 1024;
/// Ceiling on `encode_batch`: a wide claim costs memory ahead of throughput.
pub const MAX_ENCODE_BATCH: usize = 32;

pub const DEFAULT_MAX_DISTANCE: f32 = 0.52;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
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

    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db(&self) -> PathBuf {
        self.root.join(DB_FILE)
    }

    pub fn config(&self) -> PathBuf {
        self.root.join(CONFIG_FILE)
    }

    pub fn models(&self) -> PathBuf {
        self.root.join(MODELS_DIR)
    }

    pub fn history(&self) -> PathBuf {
        self.root.join(HISTORY_FILE)
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(self.models())?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: String,
    pub revision: String,
    pub cache_dir: String,
    pub offline: bool,
    pub dim: usize,
    pub precision: String,
    pub device: String,
    pub instruction: String,
    pub default_k: usize,
    pub default_max_distance: Option<f32>,
    pub port: u16,
    pub encode_interval_ms: u64,
    /// Documents the encode worker claims per tick. ONE batched forward pass serves the
    /// whole claim, so this is the throughput knob; `1` restores one-pass-per-document.
    pub encode_batch: usize,
    pub rebuild_max_documents: u64,
    pub rebuild_allow_over_budget: bool,
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
            encode_batch: 4,
            rebuild_max_documents: 2_000,
            rebuild_allow_over_budget: false,
        }
    }
}

impl Config {
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
        if self.encode_batch == 0 || self.encode_batch > MAX_ENCODE_BATCH {
            return Err(Error::Config(format!(
                "encode_batch must be 1..={MAX_ENCODE_BATCH} (got {})",
                self.encode_batch
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
