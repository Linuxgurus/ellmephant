//! Integration: the P5 read path against a real store file and real `vec0` tables.
//!
//! Runs offline — the deterministic [`HashEmbedder`] stands in for Qwen3. It is a
//! bag-of-words model, so "meaning" here reduces to shared tokens; that is enough to prove
//! the constraints run and hydrate without loading weights.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::search::{self, Field, Fields, Hit, MatchMode, Neighbor, Query};
use ellmephant::store::{self, Memory};
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
    store::add(conn, embedder, &memory(key, key, body, tags)).expect("add");
}

/// A query with no constraints beyond the phrase, overridable by the caller.
fn query(text: Option<&str>) -> Query {
    Query {
        text: text.map(str::to_string),
        pattern: None,
        mode: MatchMode::Literal,
        k: 10,
        max_distance: None,
        expand: 0,
        tags: Vec::new(),
        min_chars: None,
        max_chars: None,
        coherent: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
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

    let hits = search::run(&conn, Some(&embedder), &query(Some("alpha beta gamma")))
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
    // This replaces `tags_and_key_are_embedded`, which pinned the OLD projection (tags folded into
    // the document). Tags left the projection (§6/§9): a tag is *filing*, answered exactly by the
    // `tags` filter, and a tag-only write now re-embeds nothing. What the vector carries is the
    // document's own text — `key`, title, body (see `embed::projection`'s unit tests).
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
    let err = search::run(&conn, None, &query(Some("anything"))).unwrap_err();
    assert!(format!("{err}").contains("needs an embedder"));
}

#[test]
fn max_distance_filters_the_semantic_leg() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "alpha-note", "alpha beta gamma", &[]);
    seed(&conn, &embedder, "other-note", "epsilon zeta eta", &[]);

    // Cosine distance: the exact document is ~0, the unrelated one is ~1.
    let mut q = query(Some("alpha beta gamma"));
    q.max_distance = Some(0.5);
    let hits = search::run(&conn, Some(&embedder), &q)
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

    let bundle = search::run(&conn, Some(&embedder), &query(Some("zebra"))).expect("search");
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

    // Outgoing: note-a -> note-b (hop 1) -> note-c (hop 2).
    let mut q = query(Some("start here"));
    q.k = 1; // only the top hit seeds the expansion
    q.expand = 2;
    let bundle = search::run(&conn, Some(&embedder), &q).expect("search");
    assert_eq!(bundle.hits[0].key, "note-a");
    let keys: Vec<(&str, &str, u8)> = bundle
        .neighbors
        .iter()
        .map(|n| (n.memory.key.as_str(), n.via.as_str(), n.hop))
        .collect();
    assert_eq!(keys, vec![("note-b", "note-a", 1), ("note-c", "note-b", 2)]);

    // Incoming: note-c is reached from its backlink note-b.
    let mut q = query(Some("terminal note"));
    q.k = 1;
    q.expand = 1;
    let bundle = search::run(&conn, Some(&embedder), &q).expect("search");
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

    let hits = search::run(&conn, None, &query(None)).expect("search").hits;
    assert_eq!(
        hits.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[test]
fn wire_json_is_positional_with_a_cols_header() {
    let bundle = search::Bundle {
        hits: vec![Hit {
            id: 1,
            key: "k".into(),
            title: "T".into(),
            tags: vec!["a".into(), "b".into()],
            coherent: Some(true),
            chars: 3,
            distance: Some(0.42),
        }],
        neighbors: vec![],
    };
    // Every field requested: the full six-column shape, in canonical order.
    let all = Fields::selection(Field::ALL);
    let wire = search::to_wire_json(&bundle, &all).expect("wire");
    let v: serde_json::Value = serde_json::from_str(&wire).expect("json");
    assert_eq!(
        v["cols"],
        serde_json::json!(["key", "title", "tags", "coherent", "chars", "distance"])
    );
    assert_eq!(
        v["hits"][0],
        serde_json::json!(["k", "T", ["a", "b"], true, 3, 0.42])
    );
    assert!(
        v.get("neighbors").is_none(),
        "no neighbours by default: {wire}"
    );
    // Field names appear once, in the header — never per row.
    assert!(!wire.contains("\"key\":"), "no per-row keys: {wire}");
    assert!(!wire.contains("\"body\""), "hits carry no body: {wire}");
    // One row per line, so a raw result is scannable rather than a wall of blob.
    assert_eq!(wire.lines().count(), 3, "open+row+close: {wire}");
}

#[test]
fn wire_json_defaults_to_key_only() {
    let bundle = search::Bundle {
        hits: vec![Hit {
            id: 1,
            key: "k".into(),
            title: "T".into(),
            tags: vec!["a".into()],
            coherent: Some(true),
            chars: 3,
            distance: Some(0.42),
        }],
        neighbors: vec![],
    };
    // Nothing asked for: a hit is just its key — the lean default (SPEC §10).
    let wire = search::to_wire_json(&bundle, &Fields::key_only()).expect("wire");
    let v: serde_json::Value = serde_json::from_str(&wire).expect("json");
    assert_eq!(v["cols"], serde_json::json!(["key"]));
    assert_eq!(v["hits"][0], serde_json::json!(["k"]));
}

#[test]
fn fields_always_include_key_and_keep_canonical_order() {
    // `key` leads even when unrequested, and the rest come in canonical order regardless of the
    // order they were named in.
    let fields = Fields::selection([Field::Distance, Field::Title]);
    assert_eq!(
        fields.names().collect::<Vec<_>>(),
        ["key", "title", "distance"]
    );

    // An empty selection still yields `key`.
    let empty = Fields::selection(Vec::new());
    assert_eq!(empty.names().collect::<Vec<_>>(), ["key"]);

    // Wire names round-trip through `parse`.
    for f in Field::ALL {
        assert_eq!(Field::parse(f.name()), Some(f));
    }
    assert_eq!(Field::parse("nope"), None);
}

#[test]
fn wire_json_emits_a_neighbors_table_when_expanded() {
    let bundle = search::Bundle {
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
    };
    let wire = search::to_wire_json(&bundle, &Fields::key_only()).expect("wire");
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

    // No phrase: a pure filter. The tag keeps the k8s rows, the size floor keeps the long one.
    let mut q = query(None);
    q.tags = vec!["k8s".into()];
    q.min_chars = Some(100);
    let hits = search::run(&conn, None, &q).expect("search").hits;
    assert_eq!(
        hits.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(),
        ["long-k8s"]
    );
    assert!(hits[0].chars >= 100, "{}", hits[0].chars);
}

#[test]
fn coherent_filter_selects_by_verdict() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed(&conn, &embedder, "a", "one", &[]);
    seed(&conn, &embedder, "b", "two", &[]);
    store::set_coherent(&conn, "a", Some(true)).expect("mark");

    let keys = |c: search::Coherent| {
        let mut q = query(None);
        q.coherent = Some(c);
        search::run(&conn, None, &q)
            .expect("search")
            .hits
            .into_iter()
            .map(|h| h.key)
            .collect::<Vec<_>>()
    };

    assert_eq!(keys(search::Coherent::True), ["a"]);
    assert_eq!(keys(search::Coherent::Null), ["b"]);
    assert_eq!(keys(search::Coherent::NotNull), ["a"]);
    assert_eq!(keys(search::Coherent::NotTrue), ["b"]);
    assert_eq!(keys(search::Coherent::False), Vec::<String>::new());
}

/// The keys of a query's hits, in order.
fn hit_keys(conn: &Connection, q: &Query) -> Vec<String> {
    search::run(conn, None, q)
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
    // A literal the semantic leg cannot reach, and case-insensitive.
    q.pattern = Some("LOCALHOST:8888".into());
    let hits = search::run(&conn, None, &q).expect("search").hits;
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
    q.pattern = Some(r"err-(4|5)\d\d".into()); // matches the *key*
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
    let err = search::run(&conn, None, &q).unwrap_err();
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
    assert_eq!(hit_keys(&conn, &q), ["a", "b"]); // every row, key order
}

#[test]
fn the_lexical_leg_intersects_the_semantic_phrase() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    // Both are semantically near "alpha beta gamma"; only one holds the literal.
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
    let hits = search::run(&conn, Some(&embedder), &q)
        .expect("search")
        .hits;
    assert_eq!(
        hits.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(),
        ["ports-a"]
    );
    // The survivor kept its semantic distance (it is still a ranked hit).
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
    // The stored ISO shape is what makes a lexicographic compare correct, and `add` honours a
    // stated timestamp — so the fixture can set its own clock.
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
        store::add(&conn, &embedder, &m).expect("add");
    }

    // A `created` window: only `mid` was created in June.
    let mut q = query(None);
    q.created_after = Some("2026-06-01".into());
    q.created_before = Some("2026-06-30".into());
    assert_eq!(hit_keys(&conn, &q), ["mid"]);

    // `updated_after`: `old` never moved past January.
    let mut q = query(None);
    q.updated_after = Some("2026-09-01 00:00:00".into());
    assert_eq!(hit_keys(&conn, &q), ["mid", "new"]);

    // A window containing nothing matches NOTHING — not everything.
    let mut q = query(None);
    q.created_after = Some("2030-01-01".into());
    assert!(hit_keys(&conn, &q).is_empty());

    // Bounds compose with the semantic phrase by intersection (this leg needs the encoder).
    let mut q = query(Some("fact"));
    q.updated_before = Some("2026-02-01".into());
    let bundle = search::run(&conn, Some(&embedder), &q).expect("search");
    assert_eq!(
        bundle
            .hits
            .iter()
            .map(|h| h.key.as_str())
            .collect::<Vec<_>>(),
        ["old"]
    );
}
