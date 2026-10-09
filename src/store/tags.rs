//! Tag normalisation and the tag vocabulary (SPEC §9).
//!
//! Tags live as a comma-joined list on each `memory` row, so normalisation is the only
//! invariant: it keeps names delimiter-safe (`normalize` drops commas) and comparable.
//! [`encode`]/[`decode`] are the one codec for that column.

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

/// Encode a tag set as the `memory.tags` column value: normalised, sorted and de-duped so
/// re-ordering the input never changes the stored string (and so never forces a re-embed).
pub fn encode(tags: &[String]) -> String {
    let mut tags: Vec<String> = tags.iter().filter_map(|t| normalize(t)).collect();
    tags.sort();
    tags.dedup();
    tags.join(",")
}

/// Decode a `memory.tags` column value back into names — the inverse of [`encode`].
pub fn decode(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Every tag with its reference count, alphabetical — derived from the `memory.tags` column.
pub fn list(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare("SELECT tags FROM memory")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for row in rows {
        for tag in decode(&row?) {
            *counts.entry(tag).or_default() += 1;
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

    #[test]
    fn encode_sorts_and_dedupes_then_decode_round_trips() {
        let raw = vec!["K8s".to_string(), "infra".to_string(), "k8s".to_string()];
        assert_eq!(encode(&raw), "infra,k8s");
        assert_eq!(
            decode("infra,k8s"),
            vec!["infra".to_string(), "k8s".to_string()]
        );
        assert!(decode("").is_empty());
    }
}
