//! The two legs, fused (soma/ellmephant#62).
//!
//! The semantic leg answers "means this"; the word index answers "says this". These tests pin the
//! three shapes that justify keeping both, and the one that proves the prize: **a document with no
//! vector at all is still findable.**

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::{HashEmbedder, VecSpec};
use ellmephant::search::{self, Query};
use ellmephant::store::{self, Memory};

fn mem(key: &str, title: &str, body: &str) -> Memory {
    Memory {
        key: key.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

fn q(text: &str, k: usize) -> Query {
    Query {
        text: Some(text.into()),
        pattern: None,
        mode: Default::default(),
        k,
        max_distance: None,
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

/// ★ THE MEASUREMENT THAT STARTED THIS: `runner has no cargo` ranked the right memory FIFTH, 0.015
/// behind an unrelated one. An identifier query must be answered by the word leg, not by cosine noise.
#[test]
fn an_exact_identifier_is_found_by_the_word_leg() {
    let (_d, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    let _spec = VecSpec::of(&embedder);

    for (k, body) in [
        ("ci-images", "the runner image has no rust toolchain"),
        ("hand-notes", "a hand runs fingers in pods"),
        (
            "pr53-verification",
            "checked the migration against a clean clone",
        ),
    ] {
        common::add_now(&conn, &embedder, &mem(k, k, body)).expect("add");
    }

    let bundle = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &q("pr53-verification", 5),
    )
    .expect("search");
    assert!(
        !bundle.hits.is_empty(),
        "the identifier must find something"
    );
    assert_eq!(
        bundle.hits[0].key,
        "pr53-verification",
        "the row that SAYS the identifier must come first: {:?}",
        bundle.hits.iter().map(|h| &h.key).collect::<Vec<_>>()
    );
}

/// The other half: a phrase sharing no words with any document still returns the semantic
/// neighbourhood, because the lexical leg simply finds nothing and the fusion degrades to the vector
/// ordering. (With the hash embedder "semantic" is arithmetic, not meaning — what is under test here
/// is that an empty lexical leg does not empty the result.)
#[test]
fn a_phrase_with_no_lexical_match_still_returns_vector_hits() {
    let (_d, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    let _spec = VecSpec::of(&embedder);
    for k in ["alpha", "beta", "gamma"] {
        common::add_now(&conn, &embedder, &mem(k, k, "a body with ordinary words")).expect("add");
    }

    let bundle = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &q("zebra calculus", 3),
    )
    .expect("search");
    assert!(
        !bundle.hits.is_empty(),
        "a phrase with no lexical hit must still be answered by the vector leg"
    );
    assert!(
        bundle.hits.iter().all(|h| h.distance.is_some()),
        "these are vector hits, so they carry a distance: {:?}",
        bundle.hits.iter().map(|h| h.distance).collect::<Vec<_>>()
    );
}

/// ★ THE PRIZE (issue #62): a document that was never embedded is still findable, because the word
/// index does not need a model. `distance` is `None` for it — BM25 is not a distance, and reporting one
/// would repeat the unit confusion #59 exists to prevent.
#[test]
fn a_document_with_no_vector_is_still_findable_lexically() {
    let (_d, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    let _spec = VecSpec::of(&embedder);

    common::add_now(
        &conn,
        &embedder,
        &mem("settled", "Settled", "an indexed document"),
    )
    .expect("add");
    // Written but deliberately NOT settled: no vector exists for it.
    store::add(
        &conn,
        &mem(
            "lexical-only",
            "Lexical only",
            "the words survive without a model",
        ),
    )
    .expect("write");

    let bundle = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &q("lexical-only", 5),
    )
    .expect("search");
    let hit = bundle
        .hits
        .iter()
        .find(|h| h.key == "lexical-only")
        .unwrap_or_else(|| {
            panic!(
                "a non-embedded document must be findable: {:?}",
                bundle.hits.iter().map(|h| &h.key).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        hit.distance, None,
        "a BM25 hit must NOT carry a cosine distance — the units are opposite"
    );
}

/// A structured filter still intersects a FUSED result: the legs rank, the predicate decides.
#[test]
fn a_tag_filter_still_applies_to_a_fused_search() {
    let (_d, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(8);
    let _spec = VecSpec::of(&embedder);

    let mut tagged = mem("tagged", "Tagged", "velero restores from the bucket");
    tagged.tags = vec!["backup".into()];
    common::add_now(&conn, &embedder, &tagged).expect("add");
    common::add_now(
        &conn,
        &embedder,
        &mem("untagged", "Untagged", "velero restores from the bucket"),
    )
    .expect("add");

    let mut query = q("velero", 5);
    query.tags = vec!["backup".into()];
    let bundle = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &query,
    )
    .expect("search");
    assert_eq!(
        bundle.hits.len(),
        1,
        "{:?}",
        bundle.hits.iter().map(|h| &h.key).collect::<Vec<_>>()
    );
    assert_eq!(bundle.hits[0].key, "tagged");
}
