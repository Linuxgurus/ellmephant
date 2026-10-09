//! `store != embed`: the row keeps the full body, the vector is computed over a
//! condensed projection with wiki links stripped (SPEC §6, §8).

use std::sync::LazyLock;

use regex::Regex;

/// `[[key]]` where key is `[a-z0-9-]+` (SPEC §8).
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[[a-z0-9-]+\]\]").expect("link regex is valid"));

/// Remove every wiki link token from `s`.
pub fn strip_links(s: &str) -> String {
    LINK_RE.replace_all(s, "").to_string()
}

/// The text actually handed to the embedder: `title` then link-stripped `body`.
pub fn embed_text(title: &str, body: &str) -> String {
    let body = strip_links(body);
    let body = body.trim();
    let title = title.trim();
    if body.is_empty() {
        title.to_string()
    } else {
        format!("{title}\n{body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_wiki_links() {
        assert_eq!(strip_links("see [[infra-gitops]] now"), "see  now");
    }

    #[test]
    fn keeps_non_key_brackets() {
        let s = "an array [[Not A Key]] and [[a-b-2]]";
        assert_eq!(strip_links(s), "an array [[Not A Key]] and ");
    }

    #[test]
    fn joins_title_and_body() {
        assert_eq!(
            embed_text(" Title ", "Body [[x]] tail"),
            "Title\nBody  tail"
        );
    }

    #[test]
    fn empty_body_is_title_only() {
        assert_eq!(embed_text("Only", "   "), "Only");
    }
}
