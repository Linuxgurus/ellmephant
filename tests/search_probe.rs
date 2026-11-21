mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::{HashEmbedder, VecSpec};
use ellmephant::search::{self, Query, Via};
use ellmephant::store::Memory;
use ellmephant::store::audience::Reader;

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
        key: None,
        key_prefix: false,
        title: None,
        status: None,
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
                page.hits
                    .iter()
                    .map(|h| (&h.key, h.via))
                    .collect::<Vec<_>>()
            );
        }
    }
}

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
