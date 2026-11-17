//! Integration: the `memory_health` surface over a real store file.
//!
//! Every expected number is derived from the fixture independently — sums of the bodies and links
//! written below, not from a second call to the implementation. Vectors are written directly so the
//! duplicate pair is exact, and the private-row test proves shape (a) rather than assuming it.

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

/// Write and settle a row, returning its id.
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

    // Eight operational-log rows, one per prefix. Their bodies are the only source of `log_chars`.
    let log_bodies = [
        "alerts log body",        // 15
        "hygiene log body",       // 16
        "recess log body",        // 15
        "maintenance log body",   // 20
        "pr-watch log body",      // 17
        "dispatch log body",      // 17
        "offsite log body",       // 16
        "cluster-watch log body", // 22
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

    // A link: note-src -> note-dst. That is one outbound and one inbound row, two woven rows.
    let note_src = "see [[note-dst]]";
    let note_dst = "target body";
    let src_id = add(&conn, &embedder, &memory("note-src", note_src, &["link"]));
    let dst_id = add(&conn, &embedder, &memory("note-dst", note_dst, &["link"]));

    // One genuine duplicate pair (identical bodies) plus one singleton tag.
    let dup_body = "duplicate body";
    let dup_a = add(&conn, &embedder, &memory("dup-a", dup_body, &["dup"]));
    let dup_b = add(&conn, &embedder, &memory("dup-b", dup_body, &[]));

    // The independent expected values, straight from the fixture.
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

    // Two rows are older than 30 days: one never read, one read twice. cold30 is therefore 50%.
    conn.execute(
        "UPDATE memory SET created = datetime('now', '-40 days') \
          WHERE key IN ('alerts-2026-11-01', 'note-dst')",
        [],
    )
    .expect("age two rows");
    store::record_visit(&conn, "note-dst").unwrap();
    store::record_visit(&conn, "note-dst").unwrap();

    // Exact geometry: dup-a and dup-b are the same vector; every other row is orthogonal to both and
    // to every other row, so the only pair inside 0.05 is the duplicate.
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

    assert_eq!(h.dup_pairs, 1);
    assert_eq!(h.dup_pairs_min_distance, Some(0.0));
    approx(h.woven, 2.0 / rows as f64 * 100.0);
    approx(h.woven_inbound, 1.0 / rows as f64 * 100.0);
    approx(h.woven_outbound, 1.0 / rows as f64 * 100.0);

    assert_eq!(h.cold30_population, 2);
    assert_eq!(h.cold30, Some(50.0));

    // Tags: `link` x2, `dup` x1; nine rows are bare.
    assert_eq!(h.tag_distinct, 2);
    assert_eq!(h.tag_singletons, 1);
    assert_eq!(h.rows_untagged, 9);

    assert_eq!(h.edges, 1);
    assert_eq!(h.dangling, 0);
    assert!((h.edges_per_row - 1.0 / rows as f64).abs() < 1e-9);
    assert!(h.suppressed.is_empty(), "{:?}", h.suppressed);
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
    // An audience must name a pen that exists; `private: true` is written TO someone.
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

    // James writes a private row with a distinctive size, tag and link. Stet must not see any of it.
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

    // The row is real: its author sees one more row and a larger body.
    let james = health::compute_seen(&conn, &Reader::pen("james")).expect("health as james");
    assert_eq!(james.rows, after_stet.rows + 1);
    assert!(james.body_chars > after_stet.body_chars);
    assert!(james.tag_distinct > after_stet.tag_distinct);
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
    // Near misses: the prefix must be at the START of the key, and `alert-` is not `alerts-`.
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
    add(&conn, &embedder, &memory("notes", &"z".repeat(50), &[]));

    let h = health::compute_seen(&conn, &Reader::everything()).expect("health");
    assert_eq!(h.log_chars, 80, "eight rows x ten characters");
    assert_eq!(h.body_chars, 80 + 100 + 100 + 50);
    assert!((h.log_share.unwrap() - (80.0 / 330.0 * 100.0)).abs() < 1e-9);
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
