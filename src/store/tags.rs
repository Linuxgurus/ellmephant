//! Tag normalisation and the tag vocabulary (SPEC §9).
//!
//! Tags live as a comma-joined list on each `memory` row, so normalisation is the only
//! invariant: it keeps names delimiter-safe (`normalize` drops commas) and comparable.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::error::Result;

/// Canonicalise a tag: trim, lowercase, collapse internal whitespace to `-`, and replace any
/// comma so a name can never break the stored comma-joined list. Returns `None` for tags that
/// would be empty.
pub fn normalize(raw: &str) -> Option<String> {
    let t = raw
        .trim()
        .to_lowercase()
        .replace(char::is_whitespace, "-")
        .replace(',', "-");
    let t = t.trim_matches('-');
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Every tag with its reference count, alphabetical — derived from the `memory.tags` column.
pub fn list(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare("SELECT tags FROM memory")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for row in rows {
        for tag in row?.split(',').filter(|s| !s.is_empty()) {
            *counts.entry(tag.to_string()).or_default() += 1;
        }
    }
    Ok(counts.into_iter().collect())
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

    #[test]
    fn commas_cannot_break_the_stored_list() {
        assert_eq!(normalize("a,b").as_deref(), Some("a-b"));
    }
}
