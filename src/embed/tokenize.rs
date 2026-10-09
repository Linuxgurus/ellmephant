//! Tokenisation policy for the encoder (P3).
//!
//! Qwen3 is a *decoder* embedder: the pooled vector comes from the **last real token**,
//! so documents must be **left**-padded for that position to be meaningful (SPEC §6,
//! PLAN §1 F2). The padding side is a correctness requirement, not a preference.
//!
//! One practical wrinkle: `candle_transformers::models::qwen3::Model::forward` takes no
//! attention mask, so a *padded batch* would leak pad tokens into every real position's
//! attention. We therefore tokenise each text to its exact length and run one sequence
//! at a time (see [`crate::embed::qwen3`]); with no padding there is nothing to mask, and
//! the result matches the model card exactly.

use crate::error::{Error, Result};

/// Which side the pad tokens go on. Only `Left` is correct for this model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaddingSide {
    /// Pad on the left; the last position is always a real token.
    Left,
    /// Pad on the right.
    Right,
}

/// Tokenisation policy for the embedder.
#[derive(Debug, Clone, Copy)]
pub struct BatchConfig {
    /// Padding side; must stay `Left` for last-token pooling.
    pub padding: PaddingSide,
    /// Truncation length (special tokens included).
    pub max_len: usize,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            padding: PaddingSide::Left,
            max_len: 8192,
        }
    }
}

impl BatchConfig {
    /// Reject a policy that would silently produce wrong vectors.
    pub fn validate(&self) -> Result<()> {
        if self.padding != PaddingSide::Left {
            return Err(Error::Config(
                "last-token pooling requires left padding (SPEC §6)".into(),
            ));
        }
        if self.max_len == 0 {
            return Err(Error::Config("max_len must be > 0".into()));
        }
        Ok(())
    }
}

/// Token ids for one text, special tokens included, truncation applied by the caller's
/// tokenizer configuration (see [`crate::embed::qwen3::Qwen3Embedder::load`]).
#[cfg(feature = "encoder")]
pub fn encode(tokenizer: &tokenizers::Tokenizer, text: &str) -> Result<Vec<u32>> {
    let encoded = tokenizer
        .encode(text, true)
        .map_err(|e| Error::Config(format!("tokenize: {e}")))?;
    Ok(encoded.get_ids().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_left_padded_and_valid() {
        let cfg = BatchConfig::default();
        assert_eq!(cfg.padding, PaddingSide::Left);
        cfg.validate().unwrap();
    }

    #[test]
    fn right_padding_is_rejected() {
        let cfg = BatchConfig {
            padding: PaddingSide::Right,
            max_len: 128,
        };
        let err = cfg.validate().unwrap_err();
        assert!(format!("{err}").contains("left padding"));
    }

    #[test]
    fn zero_max_len_is_rejected() {
        let cfg = BatchConfig {
            padding: PaddingSide::Left,
            max_len: 0,
        };
        assert!(cfg.validate().is_err());
    }
}
