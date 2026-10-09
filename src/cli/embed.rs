//! The `embed` debug command and the shared encoder loader (P3).
//!
//! `load_embedder` is the one place the resolved weights directory and the `Embedder`
//! meet; both the CLI and the REPL session build their encoder through it.

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

/// `llmphant embed "…" [--query]` — load the encoder and report dim + latency (P3).
pub fn run(spec: &model::Spec, cfg: &Config, text: &str, query: bool) -> Result<()> {
    use crate::embed::Embedder;

    let started = std::time::Instant::now();
    let embedder = load_embedder(spec, cfg)?;
    let loaded = started.elapsed().as_secs_f64() * 1e3;

    let started = std::time::Instant::now();
    let vector = if query {
        embedder.embed_query(text)?
    } else {
        embedder.embed_one(text)?
    };
    let embedded = started.elapsed().as_secs_f64() * 1e3;

    // Vectors are L2-normalised, so the norm doubles as a sanity check (SPEC §6).
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    println!("model          {}", embedder.model_id());
    println!(
        "  kind         {}",
        if query { "query" } else { "document" }
    );
    println!("  dim          {}", vector.len());
    println!("  load         {loaded:.0} ms");
    println!("  embed        {embedded:.0} ms");
    println!("  norm         {norm:.6}");
    Ok(())
}
