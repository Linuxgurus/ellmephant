mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::index::vec;
use ellmephant::store::audience::Reader;
use ellmephant::store::health::{self, LOG_CLASS_PREFIXES, LOG_SHARE_DEFINITION};
use ellmephant::store::{self, Memory};

const DIM: usize = 16;

fn memory(key: &str, body: &str, tags: &[&str]) -> Memory {
    Memory {
        key: key.to_string(),
        title: format!("Title {key}"),
        body: body.to_string(),
        tags: tags.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn add(conn: &rusqlite::Connection, embedder: &HashEmbedder, mem: &Memory) -> i64 {
    common::add_now(conn, embedder, mem).expect("add memory").0
}

fn approx(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("a percentage with a non-empty denominator");
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn the_five_numbers_match_the_fixture() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    let log_bodies = [
        "alerts log body",
        "hygiene log body",
        "recess log body",
        "maintenance log body",
        "pr-watch log body",
        "dispatch log body",
        "offsite log body",
        "cluster-watch log body",
    ];
    let log_keys = [
        "alerts-2026-11-01",
        "hygiene-1",
        "recess-1",
        "maintenance-1",
        "pr-watch-1",
        "dispatch-1",
        "offsite-1",
        "cluster-watch-1",
    ];
    let mut ids = Vec::new();
    for (key, body) in log_keys.iter().zip(log_bodies.iter()) {
        ids.push(add(&conn, &embedder, &memory(key, body, &[])));
    }

    let note_src = "see [[note-dst]]";
    let note_dst = "target body";
    let src_id = add(&conn, &embedder, &memory("note-src", note_src, &["link"]));
    let dst_id = add(&conn, &embedder, &memory("note-dst", note_dst, &["link"]));

    let dup_body = "duplicate body";
    let dup_a = add(&conn, &embedder, &memory("dup-a", dup_body, &["dup"]));
    let dup_b = add(&conn, &embedder, &memory("dup-b", dup_body, &[]));

    let all_bodies = [
        log_bodies[0],
        log_bodies[1],
        log_bodies[2],
        log_bodies[3],
        log_bodies[4],
        log_bodies[5],
        log_bodies[6],
        log_bodies[7],
        note_src,
        note_dst,
        dup_body,
        dup_body,
    ];
    let total_chars: i64 = all_bodies.iter().map(|b| b.len() as i64).sum();
    let log_chars: i64 = log_bodies.iter().map(|b| b.len() as i64).sum();
    let rows = all_bodies.len() as i64;

    conn.execute(
        "UPDATE memory SET created = datetime('now', '-40 days') \
          WHERE key IN ('alerts-2026-11-01', 'note-dst')",
        [],
    )
    .expect("age two rows");
    store::record_visit(&conn, "note-dst").unwrap();
    store::record_visit(&conn, "note-dst").unwrap();

    for (i, id) in ids.iter().enumerate() {
        let mut v = vec![0.0; DIM];
        v[i + 1] = 1.0;
        vec::upsert(&conn, *id, "memory", "k", &v).unwrap();
    }
    for (id, axis) in [(src_id, 9), (dst_id, 10)] {
        let mut v = vec![0.0; DIM];
        v[axis] = 1.0;
        vec::upsert(&conn, id, "memory", "k", &v).unwrap();
    }
    let mut dup_vector = vec![0.0; DIM];
    dup_vector[0] = 1.0;
    vec::upsert(&conn, dup_a, "memory", "k", &dup_vector).unwrap();
    vec::upsert(&conn, dup_b, "memory", "k", &dup_vector).unwrap();

    let h = health::compute_seen(&conn, &Reader::everything()).expect("health");

    assert_eq!(h.scope, "visible");
    assert!(h.scope_note.contains("shape (a)"), "{}", h.scope_note);
    assert_eq!(h.rows, rows);
    assert_eq!(h.body_chars, total_chars);
    assert_eq!(h.log_chars, log_chars);
    approx(h.log_share, log_chars as f64 / total_chars as f64 * 100.0);
    assert_eq!(h.log_share_definition, LOG_SHARE_DEFINITION);

    assert_eq!(h.dup_pairs, None);
    assert_eq!(h.dup_pairs_min_distance, None);
    assert!(
        h.suppressed
            .iter()
            .any(|reason| reason.starts_with("dup_pairs:") && reason.contains("memory_dupes")),
        "suppressed must name `memory_dupes` as the finder: {:?}",
        h.suppressed
    );
    approx(h.woven, 2.0 / rows as f64 * 100.0);
    approx(h.woven_inbound, 1.0 / rows as f64 * 100.0);
    approx(h.woven_outbound, 1.0 / rows as f64 * 100.0);

    assert_eq!(h.cold30_population, 2);
    assert_eq!(h.cold30, Some(50.0));

    assert_eq!(h.tag_distinct, 2);
    assert_eq!(h.tag_singletons, 1);
    assert_eq!(h.rows_untagged, 9);

    assert_eq!(h.edges, 1);
    assert_eq!(h.dangling, 0);
    assert!((h.edges_per_row.expect("rows > 0") - 1.0 / rows as f64).abs() < 1e-9);
    assert!(
        h.suppressed
            .iter()
            .any(|reason| reason.starts_with("dup_pairs:") && reason.contains("memory_dupes")),
        "{:?}",
        h.suppressed
    );
}

#[test]
fn cold30_is_null_and_suppressed_when_no_row_is_older_than_30_days() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    add(&conn, &embedder, &memory("fresh", "fresh body", &[]));

    let h = health::compute_seen(&conn, &Reader::pen("stet")).expect("health");
    assert_eq!(h.cold30_population, 0);
    assert!(
        h.cold30.is_none(),
        "a ratio over an empty population is null, not 0: {h:?}"
    );
    let reason = h
        .suppressed
        .iter()
        .find(|r| r.starts_with("cold30:"))
        .unwrap_or_else(|| panic!("cold30 must be named in suppressed: {:?}", h.suppressed));
    assert!(
        reason.contains("no visible rows older than 30 days"),
        "{reason}"
    );
}

#[test]
fn a_second_pens_private_row_does_not_move_the_callers_numbers() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    conn.execute(
        "INSERT INTO pen (name, kind) VALUES ('stet', 'self'), ('james', 'self')",
        [],
    )
    .expect("seed pens");

    add(
        &conn,
        &embedder,
        &memory("public-a", "public body a", &["pub"]),
    );
    add(
        &conn,
        &embedder,
        &memory("public-b", "public body b", &["pub"]),
    );

    let before = health::compute_seen(&conn, &Reader::pen("stet")).expect("health before");
    assert_eq!(before.rows, 2);
    assert_eq!(before.scope, "visible");

    store::merge(
        &conn,
        &store::Patch {
            key: Some("james-private".into()),
            title: Some("james only".into()),
            body: Some(
                "private body that is very much longer than the public ones and links [[public-a]]"
                    .into(),
            ),
            private: Some(true),
            acted_by: Some("james".into()),
            asserted_by: Some("james".into()),
            tags: Some(vec!["james-tag".into()]),
            ..Default::default()
        },
    )
    .expect("private write");
    common::settle(&conn, &embedder);

    let after_stet = health::compute_seen(&conn, &Reader::pen("stet")).expect("health after");
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after_stet).unwrap(),
        "a row stet cannot read must not move a single number"
    );
    assert_eq!(after_stet.scope, "visible");

    let james = health::compute_seen(&conn, &Reader::pen("james")).expect("health as james");
    assert_eq!(james.rows, after_stet.rows + 1);
    assert!(james.body_chars > after_stet.body_chars);
    assert!(james.tag_distinct > after_stet.tag_distinct);
}

#[test]
fn a_visible_row_linking_to_a_private_target_is_not_woven() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    conn.execute(
        "INSERT INTO pen (name, kind) VALUES ('stet', 'self'), ('james', 'self')",
        [],
    )
    .expect("seed pens");

    store::merge(
        &conn,
        &store::Patch {
            key: Some("james-private".into()),
            title: Some("james only".into()),
            body: Some("private target body".into()),
            private: Some(true),
            acted_by: Some("james".into()),
            asserted_by: Some("james".into()),
            ..Default::default()
        },
    )
    .expect("private target");
    common::settle(&conn, &embedder);

    add(
        &conn,
        &embedder,
        &memory("public-src", "see [[james-private]]", &[]),
    );
    add(
        &conn,
        &embedder,
        &memory("public-other", "unrelated body", &[]),
    );

    let before = health::compute_seen(&conn, &Reader::pen("stet")).expect("health before");
    assert_eq!(
        before.rows, 2,
        "the target is private, so only the two public rows are visible"
    );
    assert_eq!(before.edges, 1, "the visible source names a target");
    assert_eq!(
        before.woven,
        Some(0.0),
        "an edge whose target stet cannot see is not woven: {before:?}"
    );
    assert_eq!(before.woven_outbound, Some(0.0), "{before:?}");
    assert_eq!(before.woven_inbound, Some(0.0), "{before:?}");

    store::merge(
        &conn,
        &store::Patch {
            key: Some("james-private".into()),
            title: Some("james only, rewritten".into()),
            body: Some("a different private target body".into()),
            private: Some(true),
            acted_by: Some("james".into()),
            asserted_by: Some("james".into()),
            ..Default::default()
        },
    )
    .expect("private rewrite");
    common::settle(&conn, &embedder);

    let after = health::compute_seen(&conn, &Reader::pen("stet")).expect("health after");
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after).unwrap(),
        "a private target must not move a single number, in either direction"
    );
}

#[test]
fn an_empty_store_suppresses_every_ratio() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let h = health::compute_seen(&conn, &Reader::pen("stet")).expect("health");
    assert_eq!(h.rows, 0);
    assert!(h.log_share.is_none(), "{h:?}");
    assert!(h.woven.is_none(), "{h:?}");
    assert!(h.woven_inbound.is_none(), "{h:?}");
    assert!(h.woven_outbound.is_none(), "{h:?}");
    assert!(h.dup_pairs_min_distance.is_none(), "{h:?}");
    assert!(
        h.edges_per_row.is_none(),
        "an empty ratio is null, not 0: {h:?}"
    );
    assert!(h.cold30.is_none(), "{h:?}");
    for name in [
        "log_share:",
        "woven:",
        "dup_pairs:",
        "edges_per_row:",
        "cold30:",
    ] {
        assert!(
            h.suppressed.iter().any(|reason| reason.starts_with(name)),
            "suppressed must name `{name}`: {:?}",
            h.suppressed
        );
    }
    let wire = serde_json::to_value(&h).unwrap();
    assert!(wire["edges_per_row"].is_null(), "{wire}");
    assert!(wire["dup_pairs"].is_null(), "{wire}");
}

#[test]
fn a_store_with_no_duplicates_still_reports_dup_pairs_as_null() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    add(&conn, &embedder, &memory("one", "first distinct body", &[]));
    add(
        &conn,
        &embedder,
        &memory("two", "second distinct body", &[]),
    );

    let h = health::compute_seen(&conn, &Reader::everything()).expect("health");
    assert_eq!(h.rows, 2);
    assert_eq!(h.dup_pairs, None, "{h:?}");
    assert!(h.dup_pairs_min_distance.is_none(), "{h:?}");
    assert!(
        h.suppressed
            .iter()
            .any(|reason| reason.starts_with("dup_pairs:") && reason.contains("memory_dupes")),
        "{:?}",
        h.suppressed
    );
}

#[test]
fn log_share_uses_exactly_the_eight_prefixes_and_carries_its_definition() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    for prefix in LOG_CLASS_PREFIXES {
        add(
            &conn,
            &embedder,
            &memory(&format!("{prefix}row"), "0123456789", &[]),
        );
    }
    add(
        &conn,
        &embedder,
        &memory("pre-alerts-row", &"x".repeat(100), &[]),
    );
    add(
        &conn,
        &embedder,
        &memory("alert-row", &"y".repeat(100), &[]),
    );
    let upper_id = add(
        &conn,
        &embedder,
        &memory("case-variant", &"u".repeat(100), &[]),
    );
    conn.execute(
        "UPDATE memory SET key = 'ALERTS-upper' WHERE id = ?1",
        [upper_id],
    )
    .expect("raw upper-case key");
    add(&conn, &embedder, &memory("notes", &"z".repeat(50), &[]));

    let h = health::compute_seen(&conn, &Reader::everything()).expect("health");
    assert_eq!(h.log_chars, 80, "eight rows x ten characters");
    assert_eq!(h.body_chars, 80 + 100 + 100 + 100 + 50);
    assert!((h.log_share.unwrap() - (80.0 / 430.0 * 100.0)).abs() < 1e-9);
    assert_eq!(
        LOG_CLASS_PREFIXES,
        &[
            "alerts-",
            "hygiene-",
            "recess-",
            "maintenance-",
            "pr-watch-",
            "dispatch-",
            "offsite-",
            "cluster-watch-",
        ]
    );
    for prefix in LOG_CLASS_PREFIXES {
        assert!(
            h.log_share_definition.contains(prefix),
            "the definition must name `{prefix}`"
        );
    }
    assert!(h.log_share_definition.contains("eight"));
}
