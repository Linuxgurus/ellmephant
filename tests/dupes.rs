//! Integration: the duplicate-candidate audit (`memory_dupes`) against real `vec0` vectors.
//!
//! Runs offline. Vectors are written directly so the distances are exact — the finder is about
//! geometry, not the embedder.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::index::vec;
use ellmephant::store::{Memory, dupes};

/// A memory with an explicit key.
fn memory(key: &str, title: &str, body: &str) -> Memory {
    Memory {
        key: key.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

#[test]
fn finds_only_the_pair_inside_the_ceiling() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);

    let (a, _) = common::add_now(&conn, &embedder, &memory("alpha", "A", "body a")).unwrap();
    let (b, _) = common::add_now(&conn, &embedder, &memory("beta", "B", "body b")).unwrap();
    let (c, _) = common::add_now(&conn, &embedder, &memory("gamma", "C", "body c")).unwrap();

    // Controlled vectors: alpha ≈ beta; gamma orthogonal to both.
    vec::upsert(&conn, a, "memory", "ka", &[1.0, 0.0, 0.0, 0.0]).unwrap();
    vec::upsert(&conn, b, "memory", "kb", &[0.9998, 0.02, 0.0, 0.0]).unwrap();
    vec::upsert(&conn, c, "memory", "kc", &[0.0, 1.0, 0.0, 0.0]).unwrap();

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs.len(), 1, "{pairs:?}");
    let p = &pairs[0];
    assert_eq!((p.key_a.as_str(), p.key_b.as_str()), ("alpha", "beta"));
    assert_eq!((p.title_a.as_str(), p.title_b.as_str()), ("A", "B"));
    assert!(p.distance < 0.01, "closer than the ceiling: {}", p.distance);

    // A ceiling below the actual distance finds nothing...
    assert!(dupes::find(&conn, 0.0001, 50).unwrap().is_empty());
    // ...and `limit` caps the list.
    assert!(dupes::find(&conn, 0.05, 0).unwrap().is_empty());
}

#[test]
fn a_pair_is_emitted_once_not_twice() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    let (a, _) = common::add_now(&conn, &embedder, &memory("one", "One", "x")).unwrap();
    let (b, _) = common::add_now(&conn, &embedder, &memory("two", "Two", "y")).unwrap();
    // Identical vectors: distance 0, and the pair must not appear from both ends.
    vec::upsert(&conn, a, "memory", "ka", &[1.0, 0.0, 0.0, 0.0]).unwrap();
    vec::upsert(&conn, b, "memory", "kb", &[1.0, 0.0, 0.0, 0.0]).unwrap();

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs.len(), 1, "one pair, not two: {pairs:?}");
    assert_eq!(pairs[0].distance, 0.0);
}

#[test]
fn an_empty_or_single_row_store_is_quiet() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    assert!(dupes::find(&conn, 0.05, 50).unwrap().is_empty());

    let embedder = HashEmbedder::new(4);
    common::add_now(&conn, &embedder, &memory("solo", "Solo", "only")).unwrap();
    assert!(dupes::find(&conn, 0.05, 50).unwrap().is_empty());
}

/// A pair of rows with controlled, identical vectors: distance 0, so the class is the only thing
/// under test.
fn pair(
    conn: &rusqlite::Connection,
    embedder: &HashEmbedder,
    key_a: &str,
    body_a: &str,
    key_b: &str,
    body_b: &str,
) {
    let (a, _) = common::add_now(conn, embedder, &memory(key_a, key_a, body_a)).unwrap();
    let (b, _) = common::add_now(conn, embedder, &memory(key_b, key_b, body_b)).unwrap();
    vec::upsert(conn, a, "memory", key_a, &[1.0, 0.0, 0.0, 0.0]).unwrap();
    vec::upsert(conn, b, "memory", key_b, &[1.0, 0.0, 0.0, 0.0]).unwrap();
}

#[test]
fn a_dated_series_is_marked_series_and_the_plan_refuses_it() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    // The same measurement on two days is near-identical prose — that is exactly why the class
    // cannot come from distance.
    let body = "offsite backup verified: 606 ids, 0 unreconstructable.";
    pair(
        &conn,
        &embedder,
        "offsite-2026-11-05",
        body,
        "offsite-2026-11-06",
        body,
    );

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs.len(), 1, "{pairs:?}");
    assert_eq!(pairs[0].class, dupes::Class::Series);

    let plan = dupes::plan(&conn, &pairs).unwrap();
    assert!(
        plan.steps.is_empty(),
        "a series is never a merge step: {plan:?}"
    );
    assert_eq!(plan.refused.len(), 1);
    assert_eq!(plan.refused[0].class, dupes::Class::Series);
    assert!(plan.refused[0].reason.contains("dated series"), "{plan:?}");
}

#[test]
fn the_same_row_twice_is_a_duplicate_and_the_plan_names_the_keeper() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    let body = "the turn boundary is offered once per wake, and a miss is not a failure.";
    pair(
        &conn,
        &embedder,
        "aaa-spine-boundary",
        body,
        "bbb-spine-boundary",
        body,
    );

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs[0].class, dupes::Class::Duplicate);

    let plan = dupes::plan(&conn, &pairs).unwrap();
    assert_eq!(plan.steps.len(), 1, "{plan:?}");
    assert_eq!(plan.steps[0].keep, "aaa-spine-boundary");
    assert_eq!(plan.steps[0].absorb, "bbb-spine-boundary");
    assert!(!plan.steps[0].reason.is_empty(), "a plan must say why");
}

#[test]
fn a_verbatim_capture_with_an_address_is_an_artifact() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    let body = "diff --git a/src/x.rs b/src/x.rs\n@@ -1,2 +1,3 @@\n-old\n+new\ncommit 643eeb8";
    pair(
        &conn,
        &embedder,
        "pr192-current-diff",
        body,
        "pr192-head-diff",
        body,
    );

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs[0].class, dupes::Class::Artifact);
    let plan = dupes::plan(&conn, &pairs).unwrap();
    assert!(
        plan.steps.is_empty(),
        "an artifact needs a cited survivor first: {plan:?}"
    );
}

#[test]
fn a_cluster_of_three_gets_one_keeper_and_executable_steps() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);

    // Three rows, one content, one vector: with NEIGHBOURS = 3 this is the ordinary mutual cluster,
    // not an edge case, and it is where a pair-per-step plan stops being executable.
    let body = "the turn boundary is offered once per wake, and a miss is not a failure.";
    for key in ["aaa-cluster", "bbb-cluster", "ccc-cluster"] {
        let (id, _) = common::add_now(&conn, &embedder, &memory(key, key, body)).unwrap();
        vec::upsert(&conn, id, "memory", key, &[1.0, 0.0, 0.0, 0.0]).unwrap();
    }

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(
        pairs.len(),
        3,
        "three mutually-close rows are three pairs: {pairs:?}"
    );

    let plan = dupes::plan(&conn, &pairs).unwrap();
    let absorbed: Vec<&str> = plan.steps.iter().map(|s| s.absorb.as_str()).collect();
    assert_eq!(absorbed.len(), 2, "a cluster of 3 is N-1 steps: {plan:?}");

    let mut unique = absorbed.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        absorbed.len(),
        "a row is absorbed TWICE: {absorbed:?}"
    );
    for step in &plan.steps {
        assert!(
            !absorbed.contains(&step.keep.as_str()),
            "keeper {} is itself absorbed: {plan:?}",
            step.keep
        );
        assert_eq!(
            step.keep, "aaa-cluster",
            "one keeper for the component: {plan:?}"
        );
        assert!(
            step.reason.contains("cluster of 3"),
            "the reason must name the cluster: {plan:?}"
        );
    }

    // The pair order is not deterministic (`find_seen` sorts by distance alone), so the plan must
    // not inherit it: the same pairs in another order must produce the same plan.
    let mut backwards = pairs.clone();
    backwards.reverse();
    let again = dupes::plan(&conn, &backwards).unwrap();
    assert_eq!(
        serde_json::to_string(&plan).unwrap(),
        serde_json::to_string(&again).unwrap(),
        "the plan must not depend on the order the pairs arrived in"
    );
}

/// The acceptance question is whether ALL the steps can be executed, not whether each step is right.
///
/// Three rows, one connected component: the finder returns `(A,B)` and `(B,C)` and nothing else. A
/// plan built per PAIR absorbs `B` twice (or absorbs the keeper that step 1 just named), which is an
/// instruction no order can follow. So this test does not inspect the plan's shape — it APPLIES the
/// plan to a simulated row set and fails on the first step that cannot be carried out.
#[test]
fn a_three_row_cluster_plan_is_executable() {
    use std::collections::{BTreeSet, HashMap};

    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);

    // Same content in every row, so every close pair is `duplicate`. The geometry is set by hand:
    // A~B and B~C sit inside the 0.05 ceiling, A~C sits outside it, so the finder's pairs are
    // exactly the two edges of one path — a component of three with no shortcut edge.
    let body = "the turn boundary is offered once per wake, and a miss is not a failure.";
    let rows_in: [(&str, [f32; 4]); 3] = [
        ("aaa-row", [1.0, 0.0, 0.0, 0.0]),
        ("bbb-row", [1.0, 0.2, 0.0, 0.0]),
        ("ccc-row", [1.0, 0.4, 0.0, 0.0]),
    ];
    for (key, vector) in rows_in {
        let (id, _) = common::add_now(&conn, &embedder, &memory(key, key, body)).unwrap();
        vec::upsert(&conn, id, "memory", key, &vector).unwrap();
    }

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    // ★ The pair SHAPE is checked, not assumed: if the finder returns something else (a star, a
    // triangle, one pair, or a refusal class), this test says so instead of quietly adapting.
    let shape: BTreeSet<(String, String)> = pairs
        .iter()
        .map(|p| (p.key_a.clone(), p.key_b.clone()))
        .collect();
    assert_eq!(
        shape,
        BTreeSet::from([
            ("aaa-row".to_string(), "bbb-row".to_string()),
            ("bbb-row".to_string(), "ccc-row".to_string()),
        ]),
        "the finder returned a different pair shape: {pairs:?}"
    );
    assert!(
        pairs.iter().all(|p| p.class == dupes::Class::Duplicate),
        "every close pair here is a duplicate: {pairs:?}"
    );

    let plan = dupes::plan(&conn, &pairs).unwrap();

    // Apply the plan IN THE ORDER PRINTED to a simulated row set, mutating it as a real merge would.
    let mut rows: HashMap<String, String> = rows_in
        .iter()
        .map(|(key, _)| (key.to_string(), body.to_string()))
        .collect();
    let inputs: BTreeSet<String> = rows.keys().cloned().collect();
    let mut absorbed: BTreeSet<String> = BTreeSet::new();
    let mut keeper_of_step_one: Option<String> = None;
    for (i, step) in plan.steps.iter().enumerate() {
        assert!(
            rows.contains_key(&step.absorb),
            "step {i} absorbs `{}`, which does not exist when the step is applied — the plan is \
             not executable in order: {plan:?}",
            step.absorb
        );
        assert!(
            rows.contains_key(&step.keep),
            "step {i} keeps `{}`, which does not exist when the step is applied: {plan:?}",
            step.keep
        );
        assert!(
            absorbed.insert(step.absorb.clone()),
            "`{}` is absorbed more than once: {plan:?}",
            step.absorb
        );
        if i == 0 {
            keeper_of_step_one = Some(step.keep.clone());
        } else if let Some(keeper) = &keeper_of_step_one {
            assert_ne!(
                &step.absorb, keeper,
                "the keeper named in step 1 (`{keeper}`) is absorbed by a later step: {plan:?}"
            );
        }
        rows.remove(&step.absorb);
    }

    assert_eq!(
        rows.len(),
        1,
        "exactly one row must survive the whole plan, found {:?}: {plan:?}",
        rows.keys().collect::<Vec<_>>()
    );
    let survivor = rows.keys().next().expect("one survivor");
    assert_eq!(
        Some(survivor),
        keeper_of_step_one.as_ref(),
        "the surviving row must be the keeper the plan names: {plan:?}"
    );

    let mut covered = absorbed;
    covered.insert(survivor.clone());
    assert_eq!(
        covered, inputs,
        "absorbed ∪ {{keeper}} must be the three input keys exactly — nothing lost, nothing \
         invented: {plan:?}"
    );
}

#[test]
fn a_capture_quoted_by_a_lesson_is_refused_not_merged() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    let capture = "```text\nerror: the gate refused\ncommit 643eeb8\n```\n";
    let lesson = format!(
        "{capture}\nThe lesson: read the log before inventing a cause.\n\
         A cache push is not a test.\n\
         A re-run that claims a cause is indistinguishable from a fix.\n"
    );
    pair(
        &conn,
        &embedder,
        "pr192-gate-log",
        capture,
        "a-lesson-about-the-gate",
        &lesson,
    );

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs[0].class, dupes::Class::Artifact);
    let plan = dupes::plan(&conn, &pairs).unwrap();
    assert!(
        plan.steps.is_empty(),
        "a lesson may quote the receipt; it must not be merged with it: {plan:?}"
    );
    assert_eq!(plan.refused.len(), 1, "{plan:?}");
}

#[test]
fn close_by_embedding_but_different_in_content_is_distinct() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(4);
    pair(
        &conn,
        &embedder,
        "offsite-retention",
        "backups are verified by opening a copy, never by the copy existing.",
        "offsite-schedule",
        "the offsite job runs at 04:30 and reports the id count it verified.",
    );

    let pairs = dupes::find(&conn, 0.05, 50).unwrap();
    assert_eq!(pairs[0].class, dupes::Class::Distinct);
    let plan = dupes::plan(&conn, &pairs).unwrap();
    assert!(plan.steps.is_empty());
    assert_eq!(plan.refused[0].class, dupes::Class::Distinct);
}
