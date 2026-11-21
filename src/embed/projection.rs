use sha2::{Digest, Sha256};

const PROBE_KEY: &str = "projection-probe";
const PROBE_TITLE: &str = "Projection probe";
const PROBE_BODY: &str = "body with a [[wiki-link]]";

pub fn fingerprint() -> String {
    let probe = embed_document(PROBE_KEY, PROBE_TITLE, PROBE_BODY);
    let mut h = Sha256::new();
    h.update(probe.as_bytes());
    crate::hex::encode(&h.finalize())
}

pub fn embed_document(key: &str, title: &str, body: &str) -> String {
    let body = crate::store::links::strip_links(body);
    [key.trim(), title.trim(), body.trim()]
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
        assert_eq!(
            crate::store::links::strip_links("see [[infra-gitops]] now"),
            "see  now"
        );
    }

    #[test]
    fn keeps_non_key_brackets() {
        let s = "an array [[Not A Key]] and [[a-b-2]]";
        assert_eq!(
            crate::store::links::strip_links(s),
            "an array [[Not A Key]] and "
        );
    }

    #[test]
    fn document_is_key_title_body_without_tags() {
        let doc = embed_document("infra-logs", "Where logs live", "Ship to ns1.");
        assert_eq!(doc, "infra-logs\nWhere logs live\nShip to ns1.");
    }

    #[test]
    fn document_strips_links_and_omits_empty_parts() {
        let doc = embed_document(" k ", "  Only title  ", "   ");
        assert_eq!(doc, "k\nOnly title");
    }
}
