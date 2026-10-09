//! Tag normalisation and governance (SPEC §15 #6).
//!
//! Tags are normalised into their own table so filtering is a join and rename is a
//! single-row update.

/// Canonicalise a tag: trim, lowercase, collapse internal whitespace to `-`.
/// Returns `None` for tags that would be empty.
pub fn normalize(raw: &str) -> Option<String> {
    let t = raw.trim().to_lowercase().replace(char::is_whitespace, "-");
    let t = t.trim_matches('-');
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_case_and_space() {
        assert_eq!(normalize("  Rust Lang ").as_deref(), Some("rust-lang"));
        assert_eq!(normalize("a  b").as_deref(), Some("a--b"));
    }

    #[test]
    fn drops_empties() {
        assert_eq!(normalize("   "), None);
        assert_eq!(normalize("--"), None);
    }
}
