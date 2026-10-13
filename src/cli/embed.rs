//! The shared encoder loader (P3).
//!
//! `load_embedder` is the one place the resolved weights directory and the `Embedder` meet;
//! the MCP session builds its encoder through it.

use crate::config::Config;
use crate::embed;
use crate::error::Result;
use crate::model;

/// Build the production Qwen3 encoder from the resolved cache + config. This is the one
/// place the weights directory and the `Embedder` meet (PLAN §6 P3).
pub(crate) fn load_embedder(
    spec: &model::Spec,
    cfg: &Config,
) -> Result<embed::qwen3::Qwen3Embedder> {
    let fetcher = model::fetch::UreqFetcher::from_env();
    let dir = model::ensure(spec, &fetcher)?;
    let opts = embed::qwen3::Options::new(cfg.dim, &cfg.device, &cfg.precision, &cfg.instruction);
    embed::qwen3::Qwen3Embedder::load(&dir, &opts)
}
