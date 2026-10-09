//! `store != embed`: the row keeps the full text, a single document is embedded, and
//! wiki links are stripped from the projection (SPEC §6, §8).

/// Remove every wiki link token from `s` (the syntax is owned by `store::links`).
pub fn strip_links(s: &str) -> String {
    crate::store::links::strip_links(s)
}

/// The text handed to the embedder for one memory: `key`, tags, `title` and `body`, one
/// non-empty part per line, with `[[links]]` stripped from the body. Embedding the whole
/// document (rather than separate fields) keeps one vector per row and lets a query match
/// on the slug or a tag as well as the prose.
pub fn embed_document(key: &str, tags: &[String], title: &str, body: &str) -> String {
    let body = strip_links(body);
    let tags = tags.join(" ");
    [key.trim(), tags.trim(), title.trim(), body.trim()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
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
    fn document_is_key_tags_title_body() {
        let tags = vec!["logs".to_string(), "observability".to_string()];
        let doc = embed_document("infra-logs", &tags, "Where logs live", "Ship to ns1.");
        assert_eq!(
            doc,
            "infra-logs\nlogs observability\nWhere logs live\nShip to ns1."
        );
    }

    #[test]
    fn document_strips_links_and_omits_empty_parts() {
        let doc = embed_document(" k ", &[], "  Only title  ", "   ");
        assert_eq!(doc, "k\nOnly title");
    }
}
