//! Integration: the project tables — identity, the two relations, and one home (SPEC §7.1).
//!
//! Runs offline against a real store file with the deterministic [`HashEmbedder`]. The sharpest
//! assertions are that a home lands in the *same transaction* as the row it describes, and that a
//! memory cannot be moved into a second project.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::store::project::{self, Home};
use ellmephant::store::{self, Memory};
use rusqlite::{Connection, params};

const DIM: usize = 8;

fn memory(key: &str, body: &str) -> Memory {
    Memory {
        key: key.to_string(),
        title: key.to_string(),
        body: body.to_string(),
        ..Default::default()
    }
}

fn project_fixture(key: &str) -> project::Project {
    project::Project {
        key: key.to_string(),
        title: key.to_string(),
        ..Default::default()
    }
}

fn seed_project(conn: &Connection, key: &str) -> String {
    project::upsert(conn, &project_fixture(key), None).expect("upsert project")
}

fn seed_memory(conn: &Connection, embedder: &HashEmbedder, key: &str) -> String {
    let (_id, key) = store::add(conn, embedder, &memory(key, "fact")).expect("add memory");
    key
}

#[test]
fn upsert_and_get_round_trip_with_defaults() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let key = project::upsert(
        &conn,
        &project::Project {
            key: "chronos".into(),
            title: "Chronos — the timing subsystem".into(),
            goal: "Place my own intent into a future I won't occupy".into(),
            state: "Design only; no bells built yet".into(),
            tags: vec!["continuity".into(), "Design".into()],
            ..Default::default()
        },
        None,
    )
    .expect("upsert");
    assert_eq!(key, "prj-chronos", "a bare name is prefixed");

    let got = project::get(&conn, "prj-chronos").unwrap().expect("exists");
    assert_eq!(got.title, "Chronos — the timing subsystem");
    assert_eq!(got.goal, "Place my own intent into a future I won't occupy");
    assert_eq!(got.status, "todo", "empty status takes the default");
    assert!(
        got.owner.is_empty(),
        "an unowned project is unassigned -- the code has no owner to default to"
    );
    assert_eq!(
        got.tags,
        vec!["continuity".to_string(), "design".to_string()]
    );
    assert!(!got.created.is_empty() && !got.updated.is_empty());

    // `prj-` is optional on the read side.
    assert!(project::get(&conn, "chronos").unwrap().is_some());
    assert!(project::get(&conn, "nope").unwrap().is_none());
}

#[test]
fn an_aborted_project_is_ended_not_gone() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "aborted".into();
    let key = project::upsert(&conn, &p, None).expect("aborted is a legal status");

    let got = project::get(&conn, &key).unwrap().expect("still there");
    assert_eq!(got.status, "aborted", "ended, not deleted");
}

#[test]
fn an_unknown_status_is_rejected() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "zombie".into();
    let err = project::upsert(&conn, &p, None).unwrap_err();
    assert!(format!("{err}").contains("unknown status"), "{err}");
    assert!(
        project::get(&conn, "prj-x").unwrap().is_none(),
        "nothing written"
    );
}

#[test]
fn an_unknown_owner_is_not_an_error() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    // No vocabulary exists to reject against: whoever writes a project owns it, and the store has
    // no business guessing who that is.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.owner = vec!["nobody-you-know".to_string()];
    let key = project::upsert(&conn, &p, None).expect("free text owner is accepted");

    let got = project::get(&conn, &key).unwrap().expect("exists");
    assert_eq!(got.owner, vec!["nobody-you-know".to_string()]);
}

#[test]
fn an_owner_set_round_trips_through_one_column() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let mut p = project_fixture("x");
    p.title = "X".into();
    p.owner = vec!["James".to_string(), "Cline".to_string()];
    project::upsert(&conn, &p, None).expect("upsert");

    let got = project::get(&conn, "prj-x").unwrap().expect("exists");
    assert_eq!(
        got.owner,
        vec!["Cline".to_string(), "James".to_string()],
        "sorted, in the spelling it was given"
    );
    let raw: String = conn
        .query_row("SELECT owner FROM project WHERE key = 'prj-x'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(raw, "Cline,James", "one column, comma-joined");
}

#[test]
fn a_key_derives_from_the_title_and_avoids_collisions() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let mut p = project_fixture("");
    p.title = "Deep Work".into();
    let first = project::upsert(&conn, &p, None).expect("first");
    let second = project::upsert(&conn, &p, None).expect("second");
    assert_eq!(first, "prj-deep-work");
    assert_eq!(second, "prj-deep-work-2", "collision gets a suffix");
}

#[test]
fn preload_is_ordered_and_replaced_wholesale() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    for key in ["a", "b", "c"] {
        seed_memory(&conn, &embedder, key);
    }
    let key = seed_project(&conn, "chronos");

    let preload = vec!["c".to_string(), "a".to_string(), "b".to_string()];
    project::upsert(&conn, &project_fixture("chronos"), Some(&preload)).expect("set preload");

    let refs = project::preload(&conn, &key).unwrap();
    let keys: Vec<&str> = refs.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, vec!["c", "a", "b"], "the ORDER is the point");
    assert!(refs.iter().all(|r| r.title.is_some()), "titles hydrate");

    // The list is ONE column on the row, in list order — no tie table, no `position`.
    let raw: String = conn
        .query_row(
            "SELECT mem_preload FROM project WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        raw, "c,a,b",
        "comma-joined memory keys, ordered by priority"
    );

    // A second `Some` replaces the list; `None` leaves it alone.
    project::upsert(&conn, &project_fixture("chronos"), Some(&["b".to_string()])).expect("replace");
    assert_eq!(project::preload(&conn, &key).unwrap().len(), 1);

    project::upsert(&conn, &project_fixture("chronos"), None).expect("touch");
    assert_eq!(
        project::preload(&conn, &key).unwrap().len(),
        1,
        "None keeps it"
    );
}

#[test]
fn preload_survives_a_reopen_in_order() {
    let (_dir, paths) = temp_store();
    let embedder = HashEmbedder::new(DIM);
    let conn = db::open(&paths).expect("open");
    seed_memory(&conn, &embedder, "a");
    seed_memory(&conn, &embedder, "b");
    let key = seed_project(&conn, "chronos");
    project::upsert(
        &conn,
        &project_fixture("chronos"),
        Some(&["b".to_string(), "a".to_string()]),
    )
    .expect("set preload");
    drop(conn);

    // The order is *stored*, not recomputed on the way out.
    let conn = db::open(&paths).expect("reopen");
    let refs = project::preload(&conn, &key).unwrap();
    let keys: Vec<&str> = refs.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, vec!["b", "a"]);
}

#[test]
fn contains_is_one_home_and_a_reupsert_keeps_it() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed_project(&conn, "chronos");
    seed_project(&conn, "sessions");

    // The memory is born in `chronos`.
    let (_id, key) = store::add_with_home(
        &conn,
        &embedder,
        &memory("a", "fact"),
        Home::Set("chronos".into()),
    )
    .expect("add with a home");
    assert_eq!(key, "a");
    let owned: Vec<String> = project::contains(&conn, "prj-chronos")
        .unwrap()
        .iter()
        .map(|r| r.key.clone())
        .collect();
    assert_eq!(owned, vec!["a"]);

    // A plain re-upsert must NOT orphan it — the whole reason membership is not a column the
    // memory write path rewrites.
    store::add(&conn, &embedder, &memory("a", "fact, edited")).expect("re-upsert");
    assert_eq!(
        project::contains(&conn, "prj-chronos").unwrap().len(),
        1,
        "the home survives a re-upsert"
    );

    // A second home is refused, and the memory is not silently moved.
    let err = store::add_with_home(
        &conn,
        &embedder,
        &memory("a", "fact"),
        Home::Set("sessions".into()),
    )
    .unwrap_err();
    assert!(format!("{err}").contains("already belongs"), "{err}");
    assert_eq!(project::contains(&conn, "prj-sessions").unwrap().len(), 0);
    assert_eq!(project::contains(&conn, "prj-chronos").unwrap().len(), 1);

    // The same home is a no-op; `Clear` detaches.
    store::add_with_home(
        &conn,
        &embedder,
        &memory("a", "fact"),
        Home::Set("prj-chronos".into()),
    )
    .expect("re-setting the same home is fine");
    store::add_with_home(&conn, &embedder, &memory("a", "fact"), Home::Clear).expect("detach");
    assert!(project::contains(&conn, "prj-chronos").unwrap().is_empty());
}

#[test]
fn a_home_that_does_not_exist_rolls_the_whole_write_back() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    // A home is a deliberate act, so a typo is an error — not a forward reference.
    let err = store::add_with_home(
        &conn,
        &embedder,
        &memory("a", "fact"),
        Home::Set("ghost".into()),
    )
    .unwrap_err();
    assert!(matches!(err, ellmephant::Error::NoProject(_)), "{err}");

    // …and because membership lands in the row's transaction, the row is not written either.
    assert!(
        store::get(&conn, "a").unwrap().is_none(),
        "the memory must not survive a rejected home"
    );
}

#[test]
fn list_filters_intersect() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let mk = |key: &str, status: &str, tags: Vec<&str>, goal: &str| project::Project {
        key: key.into(),
        title: format!("{key} project"),
        goal: goal.into(),
        status: status.into(),
        tags: tags.into_iter().map(str::to_string).collect(),
        ..Default::default()
    };
    project::upsert(
        &conn,
        &mk("prj-alpha", "active", vec!["continuity"], "ship the spine"),
        None,
    )
    .unwrap();
    project::upsert(
        &conn,
        &mk(
            "prj-beta",
            "paused",
            vec!["continuity", "storage"],
            "store the facts",
        ),
        None,
    )
    .unwrap();
    project::upsert(
        &conn,
        &mk("prj-gamma", "active", vec!["storage"], "store more"),
        None,
    )
    .unwrap();

    let all = project::list(&conn, &project::Filter::default()).unwrap();
    assert_eq!(all.len(), 3, "no filter lists every project");

    let active = project::list(
        &conn,
        &project::Filter {
            status: Some("active".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let keys: Vec<&str> = active.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(keys, vec!["prj-alpha", "prj-gamma"], "key order");

    // Tags are AND, not OR.
    let both = project::list(
        &conn,
        &project::Filter {
            tags: vec!["continuity".into(), "storage".into()],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(both.len(), 1);
    assert_eq!(both[0].key, "prj-beta");

    // The lexical leg reaches `goal` (and `state`), literal by default…
    let literal = project::list(
        &conn,
        &project::Filter {
            pattern: Some("spine".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(literal.len(), 1);
    assert_eq!(literal[0].key, "prj-alpha");

    // …or as a regex, and a blank pattern is no leg at all.
    let regex = project::list(
        &conn,
        &project::Filter {
            pattern: Some(r"store (the )?facts".into()),
            mode: ellmephant::search::MatchMode::Regex,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(regex.len(), 1);
    assert_eq!(regex[0].key, "prj-beta");
    let blank = project::list(
        &conn,
        &project::Filter {
            pattern: Some("   ".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(blank.len(), 3, "a blank pattern is not a leg");

    // Everything intersects: a status no project with that goal has = nothing.
    let none = project::list(
        &conn,
        &project::Filter {
            status: Some("paused".into()),
            pattern: Some("spine".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(none.is_empty(), "filters must intersect, not union");
}

#[test]
fn dangling_reports_forward_references_and_keeps_them_visible() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed_memory(&conn, &embedder, "present");
    let key = seed_project(&conn, "chronos");

    project::upsert(
        &conn,
        &project_fixture("chronos"),
        Some(&["present".to_string(), "later".to_string()]),
    )
    .expect("preload");

    // `later` is a forward reference: allowed, reported, never a broken FK.
    let rot = project::dangling(&conn).unwrap();
    assert_eq!(rot.len(), 1, "{rot:?}");
    assert_eq!(rot[0].to_key, "later");
    assert_eq!(rot[0].from_keys, vec![key.clone()]);

    // …and `project_get` shows it rather than dropping the row.
    let view = project::view(&conn, &key).unwrap().unwrap();
    assert_eq!(view.preload.len(), 2, "the dangling entry is still listed");
    let later = view.preload.iter().find(|r| r.key == "later").unwrap();
    assert!(later.title.is_none(), "a missing target reads as null");

    // Writing the memory clears the rot.
    seed_memory(&conn, &embedder, "later");
    assert!(project::dangling(&conn).unwrap().is_empty());
}

#[test]
fn the_wire_shapes_are_positional_and_self_describing() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed_memory(&conn, &embedder, "a");
    let key = seed_project(&conn, "chronos");
    project::upsert(&conn, &project_fixture("chronos"), Some(&["a".to_string()])).unwrap();
    store::add_with_home(
        &conn,
        &embedder,
        &memory("b", "fact"),
        Home::Set("chronos".into()),
    )
    .unwrap();

    let list = project::list(&conn, &project::Filter::default()).unwrap();
    let wire = project::list_to_wire_json(&list).unwrap();
    let v: serde_json::Value = serde_json::from_str(&wire).expect("valid json");
    assert_eq!(
        v["cols"],
        serde_json::json!(["key", "status", "owner", "tags", "title", "goal"])
    );
    assert_eq!(v["projects"][0][0], serde_json::json!("prj-chronos"));
    assert!(!wire.contains("\"key\":"), "no per-row keys: {wire}");

    let view = project::view(&conn, &key).unwrap().unwrap();
    let wire = project::view_to_wire_json(&view).unwrap();
    let v: serde_json::Value = serde_json::from_str(&wire).expect("valid json");
    assert_eq!(v["project"]["key"], serde_json::json!("prj-chronos"));
    assert_eq!(v["preload"]["cols"], serde_json::json!(["key", "title"]));
    assert_eq!(v["preload"]["rows"][0][0], serde_json::json!("a"));
    assert_eq!(
        v["contains"]["rows"][0][0],
        serde_json::json!("b"),
        "only the homed memory is contained"
    );
}
