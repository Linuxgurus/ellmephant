mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::search::{self, Field, Fields, Hit, MatchMode, Neighbor, Query};
use ellmephant::store::Memory;
use ellmephant::wire::search::to_wire_json;
use rusqlite::Connection;

const DIM: usize = 64;

fn memory(key: &str, title: &str, body: &str, tags: &[&str]) -> Memory {
    Memory {
        key: key.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        tags: tags.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn seed(conn: &Connection, embedder: &HashEmbedder, key: &str, body: &str, tags: &[&str]) {
    common::add_now(conn, embedder, &memory(key, key, body, tags)).expect("add");
}

fn query(text: Option<&str>) -> Query {
    Query {
        text: text.map(str::to_string),
        pattern: None,
        mode: MatchMode::Literal,
        k: 10,
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
        orphans: false,
        untagged: false,
        title: None,
        status: None,
    }
}

#[test]
fn semantic_ranks_the_closest_document_first() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "alpha-note",
        "alpha beta gamma delta",
        &[],
    );
    seed(
        &conn,
        &embedder,
        "other-note",
        "epsilon zeta eta theta",
        &[],
    );

    let hits = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &query(Some("alpha beta gamma")),
    )
    .expect("search")
    .hits;

    assert_eq!(hits[0].key, "alpha-note");
    assert!(
        hits[0].distance.is_some(),
        "a semantic hit must report a cosine distance"
    );
}

#[test]
fn a_tag_is_reached_by_the_filter_not_by_semantics() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "db-backup",
        "snapshot nightly to cold storage",
        &["postgres"],
    );
    seed(
        &conn,
        &embedder,
        "web-note",
        "the frontend lives in apps/web",
        &[],
    );

    let mut q = query(None);
    q.tags = vec!["postgres".into()];
    assert_eq!(
        hit_keys(&conn, &q),
        ["db-backup"],
        "the tag finder is the exact filter, not a semantic hook"
    );
}

#[test]
fn semantic_needs_an_embedder() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let err = search::run(
        &conn,
        None,
        &ellmephant::store::audience::Reader::everything(),
        &query(Some("anything")),
    )
    .unwrap_err();
    assert!(format!("{err}").contains("needs an embedder"));
}

#[test]
fn max_distance_filters_the_semantic_leg() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "alpha-note", "alpha beta gamma", &[]);
    seed(&conn, &embedder, "other-note", "epsilon zeta eta", &[]);

    let mut q = query(Some("alpha beta gamma"));
    q.max_distance = Some(0.5);
    let hits = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &q,
    )
    .expect("search")
    .hits;
    assert!(
        hits.iter().all(|h| h.distance.unwrap_or(1.0) <= 0.5),
        "got {hits:?}"
    );
    assert!(
        hits.iter().any(|h| h.key == "alpha-note"),
        "the exact match must survive the ceiling"
    );
    assert!(
        !hits.iter().any(|h| h.key == "other-note"),
        "the unrelated document must be filtered"
    );
}

#[test]
fn hits_carry_no_body() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "long-note", &"zebra ".repeat(200), &[]);

    let bundle = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &query(Some("zebra")),
    )
    .expect("search");
    assert_eq!(bundle.hits[0].title, "long-note");
    let json = serde_json::to_string(&bundle).unwrap();
    assert!(
        !json.contains("\"body\""),
        "a search hit must not carry a body: {json}"
    );
}

#[test]
fn expand_pulls_link_neighbours_and_backlinks() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "note-a", "start here [[note-b]]", &[]);
    seed(&conn, &embedder, "note-b", "middle [[note-c]]", &[]);
    seed(&conn, &embedder, "note-c", "the terminal note", &[]);

    let mut q = query(Some("start here"));
    q.k = 1;
    q.expand = 2;
    let bundle = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &q,
    )
    .expect("search");
    assert_eq!(bundle.hits[0].key, "note-a");
    let keys: Vec<(&str, &str, u8)> = bundle
        .neighbors
        .iter()
        .map(|n| (n.memory.key.as_str(), n.via.as_str(), n.hop))
        .collect();
    assert_eq!(keys, vec![("note-b", "note-a", 1), ("note-c", "note-b", 2)]);

    let mut q = query(Some("terminal note"));
    q.k = 1;
    q.expand = 1;
    let bundle = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &q,
    )
    .expect("search");
    assert_eq!(bundle.hits[0].key, "note-c");
    assert_eq!(bundle.neighbors.len(), 1);
    assert_eq!(bundle.neighbors[0].memory.key, "note-b");
}

#[test]
fn no_constraints_returns_everything_in_key_order() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "b", "two", &[]);
    seed(&conn, &embedder, "a", "one", &[]);

    let hits = search::run(
        &conn,
        None,
        &ellmephant::store::audience::Reader::everything(),
        &query(None),
    )
    .expect("search")
    .hits;
    assert_eq!(
        hits.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[test]
fn wire_json_is_positional_with_a_cols_header() {
    let bundle = search::Bundle {
        semantic: None,
        lexical: None,
        hits: vec![Hit {
            id: 1,
            key: "k".into(),
            title: "T".into(),
            tags: vec!["a".into(), "b".into()],
            chars: 3,
            via: search::Via::Semantic,
            distance: Some(0.42),
            score: None,
        }],
        neighbors: vec![],
        search: None,
    };
    let all = Fields::selection(Field::ALL);
    let wire = to_wire_json(&bundle, &all).expect("wire");
    let v: serde_json::Value = serde_json::from_str(&wire).expect("json");
    assert_eq!(
        v["cols"],
        serde_json::json!(["key", "via", "title", "tags", "chars", "distance", "score"])
    );
    assert_eq!(
        v["hits"][0],
        serde_json::json!(["k", "semantic", "T", ["a", "b"], 3, 0.42, null])
    );
    assert!(
        v.get("neighbors").is_none(),
        "no neighbours by default: {wire}"
    );
    assert!(!wire.contains("\"key\":"), "no per-row keys: {wire}");
    assert!(!wire.contains("\"body\""), "hits carry no body: {wire}");
    assert_eq!(wire.lines().count(), 3, "open+row+close: {wire}");
}

#[test]
fn wire_json_defaults_to_key_only() {
    let bundle = search::Bundle {
        semantic: None,
        lexical: None,
        hits: vec![Hit {
            id: 1,
            key: "k".into(),
            title: "T".into(),
            tags: vec!["a".into()],
            chars: 3,
            via: search::Via::Semantic,
            distance: Some(0.42),
            score: None,
        }],
        neighbors: vec![],
        search: None,
    };
    let wire = to_wire_json(&bundle, &Fields::key_only()).expect("wire");
    let v: serde_json::Value = serde_json::from_str(&wire).expect("json");
    assert_eq!(v["cols"], serde_json::json!(["key", "via"]));
    assert_eq!(v["hits"][0], serde_json::json!(["k", "semantic"]));
}

#[test]
fn fields_always_include_key_and_keep_canonical_order() {
    let fields = Fields::selection([Field::Distance, Field::Title]);
    assert_eq!(
        fields.names().collect::<Vec<_>>(),
        ["key", "via", "title", "distance"]
    );

    let empty = Fields::selection(Vec::new());
    assert_eq!(empty.names().collect::<Vec<_>>(), ["key", "via"]);

    for f in Field::ALL {
        assert_eq!(Field::parse(f.name()), Some(f));
    }
    assert_eq!(Field::parse("nope"), None);
}

#[test]
fn wire_json_emits_a_neighbors_table_when_expanded() {
    let bundle = search::Bundle {
        semantic: None,
        lexical: None,
        hits: vec![],
        neighbors: vec![Neighbor {
            hop: 1,
            via: "seed".into(),
            memory: Memory {
                key: "linked".into(),
                title: "Linked".into(),
                body: "a snippet".into(),
                tags: vec!["t".into()],
                ..Default::default()
            },
        }],
        search: None,
    };
    let wire = to_wire_json(&bundle, &Fields::key_only()).expect("wire");
    let v: serde_json::Value = serde_json::from_str(&wire).expect("json");
    assert_eq!(
        v["neighbors"]["cols"],
        serde_json::json!(["hop", "via", "key", "title", "tags", "body"])
    );
    assert_eq!(
        v["neighbors"]["rows"][0],
        serde_json::json!([1, "seed", "linked", "Linked", ["t"], "a snippet"])
    );
}

#[test]
fn tag_and_size_filters_intersect() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    let big = format!("word {}", "x ".repeat(200));
    seed(&conn, &embedder, "long-k8s", &big, &["k8s"]);
    seed(&conn, &embedder, "short-k8s", "tiny", &["k8s"]);
    seed(&conn, &embedder, "long-web", &big, &["web"]);

    let mut q = query(None);
    q.tags = vec!["k8s".into()];
    q.min_chars = Some(100);
    let hits = search::run(
        &conn,
        None,
        &ellmephant::store::audience::Reader::everything(),
        &q,
    )
    .expect("search")
    .hits;
    assert_eq!(
        hits.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(),
        ["long-k8s"]
    );
    assert!(hits[0].chars >= 100, "{}", hits[0].chars);
}

fn hit_keys(conn: &Connection, q: &Query) -> Vec<String> {
    search::run(
        conn,
        None,
        &ellmephant::store::audience::Reader::everything(),
        q,
    )
    .expect("search")
    .hits
    .into_iter()
    .map(|h| h.key)
    .collect()
}

#[test]
fn literal_pattern_finds_a_half_remembered_string() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "mcp-port",
        "server listens on localhost:8888",
        &[],
    );
    seed(&conn, &embedder, "other", "nothing to see here", &[]);

    let mut q = query(None);
    q.pattern = Some("LOCALHOST:8888".into());
    let hits = search::run(
        &conn,
        None,
        &ellmephant::store::audience::Reader::everything(),
        &q,
    )
    .expect("search")
    .hits;
    assert_eq!(
        hits.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(),
        ["mcp-port"]
    );
    assert!(
        hits[0].distance.is_none(),
        "a lexical hit carries no cosine distance"
    );
}

#[test]
fn regex_mode_matches_over_key_title_and_body() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "err-404", "gone", &[]);
    seed(&conn, &embedder, "err-500", "boom", &[]);
    seed(&conn, &embedder, "ok-note", "fine", &[]);

    let mut q = query(None);
    q.pattern = Some(r"err-(4|5)\d\d".into());
    q.mode = MatchMode::Regex;
    assert_eq!(hit_keys(&conn, &q), ["err-404", "err-500"]);
}

#[test]
fn a_bad_regex_is_a_pattern_error() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let mut q = query(None);
    q.pattern = Some("(unclosed".into());
    q.mode = MatchMode::Regex;
    let err = search::run(
        &conn,
        None,
        &ellmephant::store::audience::Reader::everything(),
        &q,
    )
    .unwrap_err();
    assert!(format!("{err}").contains("invalid pattern"), "{err}");
}

#[test]
fn a_blank_pattern_is_no_lexical_constraint() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "a", "x", &[]);
    seed(&conn, &embedder, "b", "y", &[]);

    let mut q = query(None);
    q.pattern = Some("   ".into());
    assert_eq!(hit_keys(&conn, &q), ["a", "b"]);
}

#[test]
fn the_lexical_leg_intersects_the_semantic_phrase() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "ports-a",
        "alpha beta gamma on port 8888",
        &[],
    );
    seed(
        &conn,
        &embedder,
        "ports-b",
        "alpha beta gamma on port 9999",
        &[],
    );

    let mut q = query(Some("alpha beta gamma"));
    q.pattern = Some("8888".into());
    let hits = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &q,
    )
    .expect("search")
    .hits;
    assert_eq!(
        hits.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(),
        ["ports-a"]
    );
    assert!(hits[0].distance.is_some());
}

#[test]
fn the_lexical_leg_intersects_the_tag_filter() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "k8s-note", "uses kubectl", &["k8s"]);
    seed(&conn, &embedder, "web-note", "uses kubectl", &["web"]);

    let mut q = query(None);
    q.pattern = Some("kubectl".into());
    q.tags = vec!["k8s".into()];
    assert_eq!(hit_keys(&conn, &q), ["k8s-note"]);
}
#[test]
fn time_bounds_filter_created_and_updated() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    for (key, created, updated) in [
        ("old", "2026-01-01 00:00:00", "2026-01-01 00:00:00"),
        ("mid", "2026-06-15 12:00:00", "2026-09-01 09:00:00"),
        ("new", "2026-10-14 08:00:00", "2026-10-14 08:00:00"),
    ] {
        let mut m = memory(key, key, "a fact", &[]);
        m.created = created.into();
        m.updated = updated.into();
        common::add_now(&conn, &embedder, &m).expect("add");
    }

    let mut q = query(None);
    q.created_after = Some("2026-06-01".into());
    q.created_before = Some("2026-06-30".into());
    assert_eq!(hit_keys(&conn, &q), ["mid"]);

    let mut q = query(None);
    q.updated_after = Some("2026-09-01 00:00:00".into());
    assert_eq!(hit_keys(&conn, &q), ["mid", "new"]);

    let mut q = query(None);
    q.created_after = Some("2030-01-01".into());
    assert!(hit_keys(&conn, &q).is_empty());

    let mut q = query(Some("fact"));
    q.updated_before = Some("2026-02-01".into());
    let bundle = search::run(
        &conn,
        Some(&embedder),
        &ellmephant::store::audience::Reader::everything(),
        &q,
    )
    .expect("search");
    assert_eq!(
        bundle
            .hits
            .iter()
            .map(|h| h.key.as_str())
            .collect::<Vec<_>>(),
        ["old"]
    );
}

#[test]
fn key_is_exact_and_prefix_is_explicit() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "alpha",
        "a body that mentions beta in passing",
        &[],
    );
    seed(&conn, &embedder, "beta", "a different body", &[]);
    let reader = ellmephant::store::audience::Reader::everything();
    let mut q = query(None);
    q.key = Some("alpha".into());
    let b = search::run(&conn, None, &reader, &q).expect("run");
    assert_eq!(
        b.hits.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
        vec!["alpha".to_string()],
        "an exact key returns that row alone"
    );
    assert!(b.search.unwrap().predicates.contains(&"key".to_string()));

    let mut q = query(None);
    q.key = Some("alp".into());
    q.key_prefix = true;
    let b = search::run(&conn, None, &reader, &q).expect("run");
    assert!(b.hits.iter().any(|h| h.key == "alpha"));
    assert!(
        b.search
            .unwrap()
            .predicates
            .contains(&"key_prefix".to_string()),
        "the reply names the predicate that filtered"
    );
}

#[test]
fn title_is_scoped_to_the_title_field() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "alpha",
        "a body that mentions beta in passing",
        &[],
    );
    seed(&conn, &embedder, "beta", "a different body", &[]);
    let reader = ellmephant::store::audience::Reader::everything();
    let mut q = query(None);
    q.title = Some("beta".into());
    let b = search::run(&conn, None, &reader, &q).expect("run");
    assert_eq!(
        b.hits.iter().map(|h| h.key.clone()).collect::<Vec<_>>(),
        vec!["beta".to_string()],
        "a body mention is not a title match"
    );
}

#[test]
fn status_on_a_memory_search_is_an_error_that_names_the_field() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(
        &conn,
        &embedder,
        "alpha",
        "a body that mentions beta in passing",
        &[],
    );
    seed(&conn, &embedder, "beta", "a different body", &[]);
    let reader = ellmephant::store::audience::Reader::everything();
    let mut q = query(None);
    q.status = Some("active".into());
    let err = search::run(&conn, None, &reader, &q)
        .unwrap_err()
        .to_string();
    assert!(err.contains("status") && err.contains("PROJECTS"), "{err}");
}

#[test]
fn the_orphan_predicate_lists_what_the_stat_counts_and_retires_a_row_that_gains_an_edge() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "lonely", "nobody names me", &["mail"]);
    seed(&conn, &embedder, "root", "points at [[leaf]]", &["mail"]);
    seed(&conn, &embedder, "leaf", "named by root", &["mail"]);

    let mut q = query(None);
    q.orphans = true;
    q.k = 50;
    assert_eq!(hit_keys(&conn, &q), ["lonely"]);
    let stats = ellmephant::store::stats::compute(&conn).expect("stats");
    assert_eq!(
        stats.orphans, 1,
        "the predicate and the count read the same tables"
    );
    assert_eq!(q.k, 50);

    let mut capped = query(None);
    capped.orphans = true;
    capped.k = 1;
    assert_eq!(
        hit_keys(&conn, &capped).len(),
        1,
        "k caps a filtered listing"
    );

    let patch = ellmephant::store::Patch {
        key: Some("lonely".into()),
        body: Some("now I name [[leaf]]".into()),
        ..Default::default()
    };
    common::merge_now(&conn, &embedder, &patch).expect("link it");
    assert!(
        hit_keys(&conn, &q).is_empty(),
        "an edge retires the row with no reparse"
    );
}

#[test]
fn the_untagged_predicate_intersects_and_agrees_with_the_stat() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "linked-tagged", "[[anchor]]", &["x"]);
    seed(&conn, &embedder, "anchor", "the target", &["x"]);
    seed(&conn, &embedder, "bare", "no tag, no edge", &[]);
    seed(&conn, &embedder, "bare-linked", "[[anchor]]", &[]);

    let mut q = query(None);
    q.untagged = true;
    q.k = 50;
    assert_eq!(hit_keys(&conn, &q), ["bare", "bare-linked"]);
    let stats = ellmephant::store::stats::compute(&conn).expect("stats");
    assert_eq!(stats.untagged, 2);

    q.orphans = true;
    assert_eq!(hit_keys(&conn, &q), ["bare"], "the predicates intersect");

    let patch = ellmephant::store::Patch {
        key: Some("bare".into()),
        tags: Some(vec!["x".into()]),
        ..Default::default()
    };
    common::merge_now(&conn, &embedder, &patch).expect("tag it");
    let mut after = query(None);
    after.untagged = true;
    assert_eq!(
        hit_keys(&conn, &after),
        ["bare-linked"],
        "a new tag retires the row with no reparse"
    );
}
