//! `store != embed`: the row keeps the full text, a single document is embedded, and
//! wiki links are stripped from the projection (SPEC §6, §8).

use sha2::{Digest, Sha256};

/// A fixed probe the projection is run on. The fingerprint hashes *its output*, so the fingerprint
/// moves automatically when [`embed_document`] changes what it produces — reorder the fields,
/// change the separator, and the fingerprint follows, with no version constant to remember. The
/// probe exercises every part that can vary: a key, a title, and a body that carries a wiki link
/// (which the projection strips).
const PROBE_KEY: &str = "projection-probe";
const PROBE_TITLE: &str = "Projection probe";
const PROBE_BODY: &str = "body with a [[wiki-link]]";

/// A stable id for the current embed projection: sha256 of the probe document ([`embed_document`]
/// on the [`PROBE_KEY`] inputs).
///
/// Recorded in `meta` so a change to what the projection emits can be *detected* rather than
/// silently leaving every stored vector describing the old recipe — the failure the fingerprint
/// exists to stop ([`crate::index::vec::projection_stale`]).
pub fn fingerprint() -> String {
    let probe = embed_document(PROBE_KEY, PROBE_TITLE, PROBE_BODY);
    let mut h = Sha256::new();
    h.update(probe.as_bytes());
    crate::hex::encode(&h.finalize())
}

/// The text handed to the embedder for one memory: `key`, `title` and `body`, one non-empty part
/// per line, with `[[links]]` stripped from the body. Embedding the whole document (rather than
/// separate fields) keeps one vector per row and lets a query match on the slug as well as the
/// prose.
///
/// **Tags are deliberately absent.** A tag is how a memory is *filed*, not what it *means*: the
/// vector should describe the memory's sense, and a tag is answered by the `tags` filter, which is
/// exact. Leaving them out also means re-tagging a memory does not move its vector — a tag-only
/// write re-embeds nothing (SPEC §6).
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
        // Tags are FILING, not meaning: they are the exact `tags` filter, never the vector. The
        // signature enforces it — there is no tag argument to leak one in.
        let doc = embed_document("infra-logs", "Where logs live", "Ship to ns1.");
        assert_eq!(doc, "infra-logs\nWhere logs live\nShip to ns1.");
    }

    #[test]
    fn document_strips_links_and_omits_empty_parts() {
        let doc = embed_document(" k ", "  Only title  ", "   ");
        assert_eq!(doc, "k\nOnly title");
    }

    #[test]
    fn fingerprint_is_stable_and_hashes_the_probe_document() {
        // Self-maintaining: the fingerprint is the hash of the projection's own output, so it is
        // stable while the projection is, and moves the moment the projection moves.
        assert_eq!(fingerprint(), fingerprint(), "deterministic");
        assert_eq!(fingerprint().len(), 64, "a sha256, lowercase hex");

        let probe = embed_document(PROBE_KEY, PROBE_TITLE, PROBE_BODY);
        let mut h = Sha256::new();
        h.update(probe.as_bytes());
        assert_eq!(fingerprint(), crate::hex::encode(&h.finalize()));
        // The probe carries a link, so the projection's link-stripping is part of the fingerprint.
        assert!(!probe.contains("[[wiki-link]]"), "{probe}");
    }
}
