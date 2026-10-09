//! Left-padded batching + attention mask (P3).
//!
//! Qwen3 is a decoder embedder: documents are **left**-padded so the final position
//! holds the last real token, and the vector is read from that position (SPEC §6,
//! PLAN §1 F2). The padding side is a correctness requirement, not a preference.

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
    /// Truncation length.
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
