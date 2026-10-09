//! Wiki links: `[[key]]` in a body is parsed into `memory_link` edges (a derived
//! index) and stripped from the embed projection (SPEC §8).

use std::sync::LazyLock;

use regex::Regex;

/// Capture `[[key]]` targets, key charset `[a-z0-9-]+`.
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([a-z0-9-]+)\]\]").expect("link regex is valid"));

/// Extract link targets in first-seen order, de-duplicated.
pub fn extract_links(body: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for cap in LINK_RE.captures_iter(body) {
        let key = cap[1].to_string();
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_dedupes_in_order() {
        let body = "see [[b]] then [[a]] and [[b]] again";
        assert_eq!(extract_links(body), vec!["b", "a"]);
    }

    #[test]
    fn ignores_bad_keys() {
        assert!(
            extract_links("[[Upper Case]] [[has_underscore]] [[ok-2]]")
                .contains(&"ok-2".to_string())
        );
        assert_eq!(extract_links("[[has_underscore]]"), Vec::<String>::new());
    }

    #[test]
    fn no_links_is_empty() {
        assert!(extract_links("plain text").is_empty());
    }
}
