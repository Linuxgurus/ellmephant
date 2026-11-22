use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaddingSide {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy)]
pub struct BatchConfig {
    pub padding: PaddingSide,
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
    pub fn validate(&self) -> Result<()> {
        // BOTH sides are legal, and the choice is about WHERE the pooled token sits.
        // LEFT puts every row's last token at the fixed index `T-1`; RIGHT keeps real
        // tokens at `0..len-1` and pools each row at `len-1`, which is exactly equivalent
        // under a causal mask because a real token never reads a pad after it. The encoder
        // batches with RIGHT padding (`embed::qwen3::forward_batch`) so that no padding
        // mask is needed at all — candle-nn's CPU fused kernel refuses one for `B>1`.
        if self.max_len == 0 {
            return Err(Error::Config("max_len must be > 0".into()));
        }
        Ok(())
    }
}

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
    fn right_padding_validates_too() {
        let cfg = BatchConfig {
            padding: PaddingSide::Right,
            max_len: 128,
        };
        cfg.validate().unwrap();
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
