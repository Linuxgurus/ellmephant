pub mod projection;
pub mod qwen3;
pub mod tokenize;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::error::Result;

pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    fn model_id(&self) -> &str;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed(std::slice::from_ref(&text.to_string()))?;
        out.pop()
            .ok_or_else(|| crate::error::Error::Config("empty embed batch".into()))
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.embed_one(text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VecSpec {
    pub model_id: String,
    pub dim: usize,
}

impl VecSpec {
    pub fn new(model_id: impl Into<String>, dim: usize) -> Self {
        Self {
            model_id: model_id.into(),
            dim,
        }
    }

    pub fn of(embedder: &dyn Embedder) -> Self {
        Self::new(embedder.model_id(), embedder.dim())
    }
}

pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
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

pub(crate) fn load_embedder(
    spec: &crate::model::Spec,
    cfg: &crate::config::Config,
) -> Result<qwen3::Qwen3Embedder> {
    let fetcher = crate::model::fetch::UreqFetcher::from_env();
    let dir = crate::model::ensure(spec, &fetcher)?;
    let opts = qwen3::Options::new(cfg.dim, &cfg.device, &cfg.precision, &cfg.instruction);
    qwen3::Qwen3Embedder::load(&dir, &opts)
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
