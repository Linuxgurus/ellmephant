mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::store::{self, Memory, stats};

const DIM: usize = 8;

fn memory(key: &str, body: &str, tags: &[&str]) -> Memory {
    Memory {
        key: key.to_string(),
        title: format!("Title {key}"),
        body: body.to_string(),
        source: Some("tests/stats.rs".to_string()),
        tags: tags.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn digest_counts_spread_tags_and_links() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    common::add_now(
        &conn,
        &embedder,
        &memory("a", "links to [[b]] and [[missing]]", &["k8s", "infra"]),
    )
    .expect("add a");
    common::add_now(&conn, &embedder, &memory("b", "plain body", &["k8s"])).expect("add b");
    common::add_now(&conn, &embedder, &memory("c", "orphan body", &[])).expect("add c");

    let s = stats::compute(&conn).expect("stats");

    assert_eq!(s.memories, 3);
    assert_eq!(s.body_min, "plain body".len() as i64);
    assert_eq!(s.body_max, "links to [[b]] and [[missing]]".len() as i64);
    assert_eq!(s.with_source, 3);
    assert_eq!(s.tagged, 2);
    assert!(
        s.db_bytes > 0,
        "the digest sizes the store file itself, so the field never reads as an empty database"
    );

    assert_eq!(s.distinct_tags, 2);
    assert_eq!(s.tag_assignments, 3);
    assert_eq!(s.singleton_tags, 1, "infra is used once");
    assert_eq!(s.top_tags.first(), Some(&("k8s".to_string(), 2)));
    assert!(
        s.all_tags.is_empty(),
        "the digest stays lean: the full vocabulary is asked for, not assumed"
    );
    assert!((s.avg_tags_per_memory() - 1.0).abs() < 1e-9);
    assert!((s.avg_memories_per_tag() - 1.5).abs() < 1e-9);

    assert_eq!(s.edges, 2);
    assert_eq!(s.dangling, 1);
    assert_eq!(s.orphans, 1);
    assert_eq!(s.linked, 2);

    assert_eq!(s.link_components, 1);
    assert_eq!(s.largest_component, 2);

    assert_eq!(s.untagged, 1);

    assert_eq!(s.visits_total, 0);
    assert_eq!(s.useful_total, 0);
    assert!(s.top_useful.is_empty());

    assert!(stats::disk_bytes(&paths.db()) > 0);
}

#[test]
fn the_digest_carries_the_encode_queue() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let _spec = ellmephant::embed::VecSpec::of(&embedder);

    store::add(&conn, &memory("queued", "owed a vector", &["k8s"])).expect("add");
    let s = stats::compute(&conn).expect("stats");
    assert_eq!(s.encode_pending, 1, "the write scheduled its document");
    assert_eq!(s.encode_failed, 0, "and nothing has failed");
    assert!(
        s.encode_oldest_s.is_some(),
        "a document that is waiting has an age — this is the number that shows a stalled worker"
    );

    let processed = ellmephant::store::encode::drain(&conn, &embedder, 10).expect("drain");
    assert_eq!(processed, 1);
    let s = stats::compute(&conn).expect("stats");
    assert_eq!(s.encode_pending, 0);
    assert_eq!(s.encode_failed, 0);
    assert_eq!(
        s.encode_oldest_s, None,
        "nothing queued, so nothing is waiting"
    );
}

#[test]
fn an_empty_store_is_all_zeroes_not_a_panic() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let s = stats::compute(&conn).expect("stats");
    assert_eq!(s.memories, 0);
    assert_eq!(s.distinct_tags, 0);
    assert_eq!(s.orphans, 0);
    assert_eq!(s.oldest, None);
    assert_eq!(s.newest, None);
    assert_eq!(s.avg_body_chars(), 0.0);
    assert!(s.top_tags.is_empty());
}

#[test]
fn telemetry_totals_and_top_useful() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    common::add_now(&conn, &embedder, &memory("a", "body a", &["k8s"])).expect("add a");
    common::add_now(&conn, &embedder, &memory("b", "body b", &["k8s"])).expect("add b");

    store::record_visit(&conn, "a").unwrap();
    store::record_visit(&conn, "a").unwrap();
    store::record_visit(&conn, "b").unwrap();
    store::record_useful(&conn, "b").unwrap();
    store::record_useful(&conn, "b").unwrap();
    store::record_useful(&conn, "a").unwrap();

    let s = stats::compute(&conn).expect("stats");
    assert_eq!(s.visits_total, 3);
    assert_eq!(s.useful_total, 3);
    assert_eq!(s.top_useful.first(), Some(&("b".to_string(), 2)));
}

#[test]
fn unvoted_hot_surfaces_read_but_never_marked_useful() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    common::add_now(&conn, &embedder, &memory("hot", "hot body", &["k8s"])).expect("add hot");
    common::add_now(&conn, &embedder, &memory("cold", "cold body", &["k8s"])).expect("add cold");
    common::add_now(&conn, &embedder, &memory("voted", "voted body", &["k8s"])).expect("add voted");

    for _ in 0..3 {
        store::record_visit(&conn, "hot").unwrap();
    }
    store::record_visit(&conn, "cold").unwrap();
    store::record_visit(&conn, "voted").unwrap();
    store::record_visit(&conn, "voted").unwrap();
    store::record_useful(&conn, "voted").unwrap();

    let s = stats::compute(&conn).expect("stats");
    assert_eq!(s.unvoted_hot, vec![("hot".to_string(), 3)]);
    assert_eq!(s.useful_total, 1);
}

#[test]
fn vocabulary_is_the_whole_tag_set_count_desc_then_name() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    common::add_now(&conn, &embedder, &memory("a", "x", &["k8s", "infra"])).expect("a");
    common::add_now(&conn, &embedder, &memory("b", "y", &["k8s"])).expect("b");
    common::add_now(&conn, &embedder, &memory("c", "z", &["gotcha"])).expect("c");

    let vocab = stats::vocabulary(&conn).expect("vocabulary");
    assert_eq!(
        vocab,
        vec![
            ("k8s".to_string(), 2),
            ("gotcha".to_string(), 1),
            ("infra".to_string(), 1),
        ],
        "count-desc then name: the head is bloated, the tail is the sprawl"
    );
}
