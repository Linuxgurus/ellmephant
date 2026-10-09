//! Qwen3-Embedding encoder on candle (P3).
//!
//! Deliberately *not* implemented yet: PLAN §1 F1/F2 established that we own the
//! encoder rather than depend on `fastembed` (which drags in ONNX Runtime and
//! mean-pools). The contract is fixed here so the store/search layers can be written
//! against it.

use std::path::Path;

use crate::embed::Embedder;
use crate::error::{Error, Result};

/// Qwen3-Embedding-0.6B: 1024-d, last-token pooling, left padding, L2-normalised.
pub struct Qwen3Embedder {
    #[allow(dead_code)]
    model_dir: std::path::PathBuf,
    dim: usize,
}

impl Qwen3Embedder {
    /// Load weights from `dir`; `--offline` forbids HF download (P3).
    pub fn load(dir: &Path, dim: usize) -> Result<Self> {
        let _ = (dir, dim);
        Err(Error::NotImplemented(
            "candle Qwen3 encoder (PLAN.md P3; spike S0.2)",
        ))
    }

    /// The model id recorded in `meta`.
    pub const MODEL_ID: &'static str = "Qwen/Qwen3-Embedding-0.6B";
}

impl Embedder for Qwen3Embedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn model_id(&self) -> &str {
        Self::MODEL_ID
    }

    fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Err(Error::NotImplemented(
            "candle Qwen3 encoder (PLAN.md P3; spike S0.2)",
        ))
    }
}
