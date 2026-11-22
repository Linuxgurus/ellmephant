//! What does batching actually buy? Loads the real encoder and times the same
//! documents one-at-a-time (what the worker did before #275) against a single
//! batched forward (what it does now), at several batch sizes.
//!
//! Pin it to the worker's real CPU budget before believing the numbers:
//!
//! ```text
//! ELLMEPHANT_TEST_MODEL_DIR=~/model275 taskset -c 0,1 \
//!     cargo run --release --features encoder --example encode_bench
//! ```
//!
//! The printed `projected docs/min` is the honest companion to the recorded
//! 1.65 baseline: `batch / (batch_secs + encode_interval_ms)`, the same
//! arithmetic that produced the baseline from single passes.

use std::path::Path;
use std::time::Instant;

use ellmephant::embed::Embedder;
use ellmephant::embed::qwen3::{Options, Qwen3Embedder};

const INTERVAL_SECS: f32 = 1.0;

fn docs() -> Vec<String> {
    // Roughly the store's mean body (~2630 chars) plus short ones, so the padding
    // group is realistically ragged rather than uniformly sized.
    vec![
        "Memory is the only layer this process edits. Everything else is reloaded from \
         the context document at boot, which means a fact that lives only in the \
         conversation is at risk the moment the turn ends. The store is what I believe \
         and the only layer I edit, so a claim gets written down before the turn ends."
            .to_string(),
        "A quiet turn that finds nothing is an honest outcome; a turn that reports success \
         it has not verified is not."
            .to_string(),
        "Home Rule: inside the dir and the namespace, act without asking, any purpose. \
         Outside is blast radius rather than trust, which is why a cluster change gets a \
         name attached to it and a file change does not."
            .to_string(),
        "the encode worker claims one document per pass and sleeps a second between passes"
            .to_string(),
        "Live state is measured, never remembered: a reading taken once and preloaded every \
         turn gets read as fact hours later, and a row right about when it was read but \
         wrong about now is worse than no row at all."
            .to_string(),
        "Dangling links are a numbered, accounted-for set rather than damage: rewriting a \
         quoted token falsifies a record of what the store held."
            .to_string(),
    ]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::var("ELLMEPHANT_TEST_MODEL_DIR")
        .expect("set ELLMEPHANT_TEST_MODEL_DIR to a flat weights dir");
    // The store's real distribution: mean body ~2630 chars, max ~22k. A benchmark made
    // of two-sentence documents measures a different regime than the queue the worker
    // actually drains, so the long set is the one the baseline came from.
    let long: Vec<String> = (0..6)
        .map(|i| {
            let seed = docs().remove(0);
            let mut s = String::new();
            while s.chars().count() < 2600 {
                s.push_str(&seed);
                s.push_str(&format!(" ({i})"));
            }
            s
        })
        .collect();
    let texts = match std::env::args().nth(1).as_deref() {
        Some("short") => docs(),
        _ => long,
    };
    println!(
        "{} docs, mean {} chars",
        texts.len(),
        texts.iter().map(|t| t.chars().count()).sum::<usize>() / texts.len()
    );

    println!("loading encoder from {dir}");
    let opts = Options::new(1024, "cpu", "bf16", "");
    let t = Instant::now();
    let embedder = Qwen3Embedder::load(Path::new(&dir), &opts)?;
    println!("  loaded in {:.1}s\n", t.elapsed().as_secs_f32());

    embedder.embed_one("warm up")?;

    // BEFORE: one forward per document, which is what the worker did until #275.
    let t = Instant::now();
    for text in &texts {
        embedder.embed_one(text)?;
    }
    let one_by_one = t.elapsed().as_secs_f32();
    let per_doc_one = one_by_one / texts.len() as f32;
    println!(
        "one at a time: {:>6.1}s for {} docs -- {:>5.1}s/doc, projected {:>5.2} docs/min",
        one_by_one,
        texts.len(),
        per_doc_one,
        texts.len() as f32 / (one_by_one + texts.len() as f32 * INTERVAL_SECS) * 60.0
    );

    for batch in [2usize, 4, 8] {
        if batch > texts.len() {
            continue;
        }
        let slice = &texts[..batch];
        let t = Instant::now();
        let out = embedder.embed(slice)?;
        let secs = t.elapsed().as_secs_f32();
        assert_eq!(out.len(), batch);
        println!(
            "batch of {:>2}:   {:>6.1}s for {} docs -- {:>5.1}s/doc, projected {:>5.2} docs/min ({:.2}x)",
            batch,
            secs,
            batch,
            secs / batch as f32,
            batch as f32 / (secs + INTERVAL_SECS) * 60.0,
            (per_doc_one * batch as f32) / secs
        );
    }
    Ok(())
}
