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
