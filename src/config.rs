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

/// Default cosine-distance ceiling for the KNN leg when a search omits `max_distance` (SPEC §10).
///
/// ★ RE-CALIBRATED 2026-10-21 on the live store (Qwen3-Embedding-0.6B, dim 1024), because the old
/// 0.6 was measured on a different model and admitted every nonsense hit on this one:
///
/// | query | rank-1 | rank-5 |
/// |---|---|---|
/// | a relevant phrase | 0.338 | 0.483 |
/// | a vague-but-real phrase | 0.455 | 0.489 |
/// | four nonsense phrases | 0.529 | 0.591 |
///
/// So the separation is 0.489 -> 0.529 and **0.52** sits in it: every measured genuine hit survives,
/// nothing measured as noise does. The old comment's "noise floor ≳0.63" was false here. High-precision
/// callers can still pass `max_distance` per call (≈0.45 for confident-only). A single number cannot
/// separate "nothing similar" from "cut by the threshold" — a `match` verdict is the follow-up (#59).
pub const DEFAULT_MAX_DISTANCE: f32 = 0.52;

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
    /// The largest AUTOMATIC rebuild (a changed projection at open) this store will start, counted in
    /// documents. A rebuild is unbounded work started by a config change, so it is asked to fit a
    /// budget before it purges anything (soma/ellmephant#76).
    ///
    /// The cost of one document embed is measured at 5–11.5 s (see #62's rationale), so the default
    /// 2000 is ≈6.4 h at the slow end: today's 756-document store fits with headroom, and a
    /// hundredfold store does not. A refused rebuild writes nothing and leaves the index STALE, which
    /// is the state the store already has a name for.
    ///
    /// ★ This is a DOCUMENT proxy, not the money wallet: the daily spend ceiling lives in the spine,
    /// which ellmephant cannot read. The honest local question is "is this rebuild affordable", and
    /// the honest local unit is the work it will do.
    pub rebuild_max_documents: u64,
    /// Proceed with a rebuild that exceeds [`Config::rebuild_max_documents`], deliberately. Config,
    /// never a caller: an over-budget rebuild is a decision, and a request cannot make it.
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
            rebuild_max_documents: 2_000,
            rebuild_allow_over_budget: false,
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
