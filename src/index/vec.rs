//! `sqlite-vec` encapsulation (P2/P5).
//!
//! Everything that knows about `vec0` lives here so the extension can be swapped
//! without touching the rest of the crate (PLAN §9). Registration is
//! `sqlite3_vec_init` via `sqlite3_auto_extension` — no `.dylib`, no
//! `load_extension` (plan spike S0.1).

use crate::error::{Error, Result};

/// Register the `vec0` virtual table module on the process. Idempotent.
pub fn register_extension() -> Result<()> {
    Err(Error::NotImplemented(
        "sqlite-vec auto-extension (PLAN.md P2; spike S0.1)",
    ))
}

/// Vector search hint.
#[derive(Debug, Clone, Copy)]
pub struct Knn {
    /// Number of neighbours.
    pub k: usize,
    /// Optional cosine-distance ceiling.
    pub max_distance: Option<f32>,
}
