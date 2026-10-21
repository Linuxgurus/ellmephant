//! The embedder boundary.
//!
//! `Embedder` is a trait so the DB / search layers can be tested with a deterministic
//! fake instead of loading ~1.2 GB of Qwen3 weights (PLAN §7). The real candle
//! implementation lives in [`qwen3`] and lands in P3.

pub mod projection;
pub mod qwen3;
pub mod tokenize;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::error::Result;

/// Turns text into L2-normalised vectors.
///
/// `Send + Sync` so a loaded encoder can be shared behind a lock (the MCP server, P7).
pub trait Embedder: Send + Sync {
    /// Vector length; must be stable for a given model.
    fn dim(&self) -> usize;
    /// Stable model identifier, recorded in `meta` and folded into the row hash.
    fn model_id(&self) -> &str;
    /// Embed a batch of *documents*, one vector per input, same order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Convenience wrapper for a single document.
    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed(std::slice::from_ref(&text.to_string()))?;
        out.pop()
            .ok_or_else(|| crate::error::Error::Config("empty embed batch".into()))
    }

    /// Embed a *query*. Asymmetric models (Qwen3) prepend an instruction here; symmetric
    /// ones fall back to [`embed_one`](Self::embed_one) (SPEC §6).
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_one(text)
    }
}

/// The identity of the vector space a row's vector is written in.
///
/// The projection fingerprint needs a model id and a dimension to describe a vector space,
/// and both are *config* — not a property of loaded weights. Carrying them in a value is what lets
/// the write path hash a document **without loading 1.2 GB of encoder**, which is what the encode
/// worker is built on (see `ENCODE-WORKER.md`).
///
/// [`VecSpec::of`] is the bridge for the callers that already hold a live embedder — a re-embed, a
/// rename — so they need no config plumbed down to them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VecSpec {
    /// The model id recorded in `meta`, and part of the projection fingerprint.
    pub model_id: String,
    /// Vector length; must be stable for a given model.
    pub dim: usize,
}

impl VecSpec {
    /// A vector space identified by its model id and its dimension.
    pub fn new(model_id: impl Into<String>, dim: usize) -> Self {
        Self {
            model_id: model_id.into(),
            dim,
        }
    }

    /// The vector space a live embedder speaks.
    pub fn of(embedder: &dyn Embedder) -> Self {
        Self::new(embedder.model_id(), embedder.dim())
    }
}

/// L2-normalise in place. No-op for the zero vector.
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Deterministic bag-of-words hash embedding. Fast, offline, no model — good enough
/// to exercise storage, ranking and plumbing in tests.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    /// Build a fake embedder of the given dimension.
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    fn one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        for token in text.split_whitespace() {
            let mut h = DefaultHasher::new();
            token.to_lowercase().hash(&mut h);
            let idx = (h.finish() as usize) % self.dim;
            v[idx] += 1.0;
        }
        l2_normalize(&mut v);
        v
    }
}

impl Embedder for HashEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn model_id(&self) -> &str {
        "fake-hash-v1"
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.one(t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_is_deterministic_and_normalised() {
        let e = HashEmbedder::new(64);
        let a = e.embed_one("the quick brown fox").unwrap();
        let b = e.embed_one("the quick brown fox").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        let norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm = {norm}");
    }

    #[test]
    fn different_text_differs() {
        let e = HashEmbedder::new(64);
        assert_ne!(
            e.embed_one("alpha beta").unwrap(),
            e.embed_one("gamma delta").unwrap()
        );
    }

    #[test]
    fn empty_text_is_zero_vector() {
        let e = HashEmbedder::new(8);
        assert_eq!(e.embed_one("").unwrap(), vec![0.0f32; 8]);
    }
}
