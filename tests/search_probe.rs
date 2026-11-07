//! The known-answer probe set: does a search tell the caller *how* it was answered?
//!
//! `soma/ellmephant#59` / `#213`. A fused list is not a similarity list — the lexical leg fills the
//! page when the semantic leg has nothing, and `hits.len()` cannot say so. Before this change a
//! caller saw ten rows with `distance: null` and had no way to learn that their phrase reached
//! nothing. These probes are the regression.
//!
//! ## What this test can and cannot prove
//!
//! `HashEmbedder` is a stand-in: its vectors are not semantic, so a question phrased in words that
//! *mean* the right row will not necessarily rank it first. This test therefore asserts only the
//! invariants that hold **by construction**, whatever the embeddings are:
//!
//! 1. every hit carries a `via`;
//! 2. `distance` is present exactly when the semantic leg ranked the row;
//! 3. `semantic_only` never returns a lexical hit;
//! 4. a phrase-less, pattern-less query reports `listing`;
//! 5. the envelope cannot contradict the hits: `semantic_candidates == 0` implies no hit came from
//!    the semantic leg.
//!
//! The converse is deliberately NOT asserted — `semantic_candidates > 0` does not imply a semantic
//! hit survives into the page, because the RRF `k` cap can legitimately leave it out.
//!
//! Ranking *quality* — whether a paraphrase finds the row at all — needs real vectors and is
//! measured out of band against a live store. That is the number that must exist before
//! `DEFAULT_MAX_DISTANCE` moves; this test guarantees that a blind answer can no longer look like a
//! confident one.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::{HashEmbedder, VecSpec};
use ellmephant::search::{self, Query, Via};
use ellmephant::store::Memory;
use ellmephant::store::audience::Reader;

/// Twenty known-answer pairs. The questions avoid their answer's words on purpose: that is the
/// shape where only the vector leg can help, and the shape the ceiling was silently cutting.
const PROBES: [(&str, &str); 20] = [
    (
        "the thing that wakes a person in the night when nothing is wrong",
        "false-alarm",
    ),
    (
        "an instrument that says yes after it has stopped working",
        "green-lie",
    ),
    (
        "what I keep when the only copy is older than I think",
        "stale-backup",
    ),
    (
        "a promise I made to myself and then forgot to keep",
        "open-decision",
    ),
    (
        "the doorway that lets a stranger read my notes",
        "audience-leak",
    ),
    (
        "how long a locked door stays locked after the holder dies",
        "lease-expiry",
    ),
    (
        "the cost of asking more often than the answer can change",
        "attention-theatre",
    ),
    ("a rule I follow that nobody wrote down", "unwritten-law"),
    (
        "the smallest number that would tell me I am wrong",
        "falsifier",
    ),
    (
        "a memory that describes a thing that no longer exists",
        "tombstone",
    ),
    (
        "what a scheduler does when nobody is waiting for it",
        "heartbeat",
    ),
    (
        "the difference between silence and an empty answer",
        "absence",
    ),
    (
        "a fix I could have applied and instead reported",
        "watched-decay",
    ),
    ("the part of a sentence that carries the scope", "resolver"),
    ("how a deletion keeps its history", "version-survives"),
    (
        "a queue that is never emptied and never grows",
        "dead-letter",
    ),
    (
        "the measurement that embarrasses the person who asked for it",
        "probe-set",
    ),
    (
        "what happens when two writers believe they are one",
        "interleave",
    ),
    (
        "the ceiling nobody can see until it cuts something",
        "max-distance",
    ),
    ("a page that proves delivery and not reception", "receipt"),
];

fn mem(key: &str) -> Memory {
    Memory {
        key: key.to_string(),
        title: key.to_string(),
        body: format!("prose about {key}"),
        ..Default::default()
    }
}

fn q(text: Option<&str>, k: usize) -> Query {
    Query {
        text: text.map(str::to_string),
        pattern: None,
        mode: Default::default(),
        k,
        max_distance: None,
        semantic_only: false,
        expand: 0,
        tags: Vec::new(),
        min_chars: None,
        max_chars: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
    }
}

fn store_with_probes() -> (tempfile::TempDir, rusqlite::Connection, HashEmbedder) {
    let (dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    let _spec = VecSpec::of(&embedder);
    for (_question, key) in PROBES {
        common::add_now(&conn, &embedder, &mem(key)).expect("add");
    }
    (dir, conn, embedder)
}

fn is_semantic(v: Via) -> bool {
    matches!(v, Via::Semantic | Via::Both)
}

/// A page can no longer be blind without saying so. Run over all twenty probes, so an edit that
/// drops the labels fails everywhere rather than only on the query somebody wrote a test for.
#[test]
fn every_hit_reports_which_leg_found_it() {
    let (_dir, conn, embedder) = store_with_probes();
    let reader = Reader::everything();
    for (question, _answer) in PROBES {
        let bundle =
            search::run(&conn, Some(&embedder), &reader, &q(Some(question), 5)).expect("search");
        let meta = bundle
            .search
            .as_ref()
            .expect("every run carries SearchMeta");
        assert_eq!(meta.max_distance, None, "the ceiling this call applied");
        for h in &bundle.hits {
            assert!(
                h.distance.is_some() == is_semantic(h.via),
                "distance is present exactly when the semantic leg ranked the row: {:?} {:?}",
                h.key,
                h.via
            );
        }
        if meta.semantic_candidates == 0 {
            assert!(
                bundle.hits.iter().all(|h| !is_semantic(h.via)),
                "the envelope says the phrase leg found nothing, so no hit may claim it did: {:?}",
                bundle
                    .hits
                    .iter()
                    .map(|h| (&h.key, h.via))
                    .collect::<Vec<_>>()
            );
        }
    }
}

/// The honest-empty-list guarantee: asking for a semantic answer either returns semantic rows or
/// returns nothing — never a page of word matches dressed as an answer.
#[test]
fn semantic_only_never_returns_a_lexical_hit() {
    let (_dir, conn, embedder) = store_with_probes();
    let reader = Reader::everything();
    let mut query = q(Some("the thing that wakes a person in the night"), 10);
    query.semantic_only = true;
    let bundle = search::run(&conn, Some(&embedder), &reader, &query).expect("search");
    assert!(
        bundle.hits.iter().all(|h| is_semantic(h.via)),
        "every hit under semantic_only must be one the phrase leg ranked: {:?}",
        bundle
            .hits
            .iter()
            .map(|h| (&h.key, h.via))
            .collect::<Vec<_>>()
    );
}

/// ★ THE GUARANTEE THE FLAG EXISTS FOR, and the one it did not have at first: when the phrase leg
/// found something, `semantic_only` must return something.
///
/// The first version applied the filter AFTER `fuse`, which had already spent all `k` slots on the
/// combined ranking — so word matches could crowd every semantic row out of the page and
/// `semantic_only` returned EMPTY while `semantic_candidates` was greater than zero. The flag then
/// contradicted the envelope it was supposed to corroborate. Fixed by skipping the fusion under
/// `semantic_only` and taking the phrase leg's own top-k; this test is what keeps it fixed.
///
/// The counter-case is deliberate and NOT asserted: a structured filter (`tags`, size, time) can
/// legitimately empty the semantic page while candidates exist, because the filter removed them.
/// That is an honest empty list and a different fact from "the phrase leg answered nothing".
#[test]
fn semantic_only_is_never_empty_when_the_phrase_leg_found_something() {
    let (_dir, conn, embedder) = store_with_probes();
    let reader = Reader::everything();
    for (question, _answer) in PROBES {
        let fused =
            search::run(&conn, Some(&embedder), &reader, &q(Some(question), 5)).expect("search");
        let candidates = fused
            .search
            .as_ref()
            .map(|m| m.semantic_candidates)
            .unwrap_or(0);
        let mut only = q(Some(question), 5);
        only.semantic_only = true;
        let page = search::run(&conn, Some(&embedder), &reader, &only).expect("search");
        if candidates > 0 {
            assert!(
                !page.hits.is_empty(),
                "the phrase leg had {candidates} candidates, so semantic_only must not be empty: {question}"
            );
            assert!(
                page.hits.iter().all(|h| is_semantic(h.via)),
                "and every row on it must be the phrase leg's: {:?}",
                page.hits.iter().map(|h| (&h.key, h.via)).collect::<Vec<_>>()
            );
        }
    }
}

/// The word index must not be able to crowd the phrase leg out of its own answer: with `k` smaller
/// than the semantic candidate set, `semantic_only` still returns a full page of semantic rows.
#[test]
fn a_crowded_page_does_not_starve_semantic_only() {
    let (_dir, conn, embedder) = store_with_probes();
    let reader = Reader::everything();
    let question = "the thing that wakes a person in the night when nothing is wrong";
    let mut only = q(Some(question), 1);
    only.semantic_only = true;
    let page = search::run(&conn, Some(&embedder), &reader, &only).expect("search");
    assert_eq!(page.hits.len(), 1, "k=1 is one row, not zero");
    assert!(is_semantic(page.hits[0].via), "and it is the phrase leg's");
}

/// No phrase and no pattern is a listing, not a search — and it says so rather than looking like a
/// perfect match.
#[test]
fn a_bare_listing_reports_listing() {
    let (_dir, conn, embedder) = store_with_probes();
    let reader = Reader::everything();
    let bundle = search::run(&conn, Some(&embedder), &reader, &q(None, 5)).expect("search");
    assert!(!bundle.hits.is_empty(), "a listing has rows");
    assert!(
        bundle.hits.iter().all(|h| h.via == Via::Listing),
        "no phrase, no pattern: nothing ranked these rows"
    );
}
