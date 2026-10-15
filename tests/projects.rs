//! Integration: the project tables — identity and the one relation (SPEC §7.1).
//!
//! Runs offline against a real store file with the deterministic [`HashEmbedder`]. The sharpest
//! assertions are that a `preload` key which resolves to nothing is *reported* rather than hidden,
//! and that membership is nothing but a tag — a memory joins a project by carrying `prj:<key>`, so
//! its members are found with the ordinary `tags` filter.

mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::store::links;
use ellmephant::store::project;
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
    assert_eq!(
        key, "chronos",
        "the key is a plain name, not a prefixed one"
    );

    let got = project::get(&conn, "chronos").unwrap().expect("exists");
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

    // A `prj:`/`prj-` marker on the read side is optional — stripped, like everywhere else.
    assert!(project::get(&conn, "chronos").unwrap().is_some());
    assert!(project::get(&conn, "prj:chronos").unwrap().is_some());
    assert!(project::get(&conn, "prj-chronos").unwrap().is_some());
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
    assert!(
        !got.end_date.is_empty(),
        "aborted ends it just as done does: `status` says HOW, the date says WHEN"
    );
}

#[test]
fn a_stated_start_is_kept_and_the_end_follows_the_status() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let mut p = project_fixture("x");
    p.title = "X".into();
    project::upsert(&conn, &p, None).expect("upsert");
    let fresh = project::get(&conn, "x").unwrap().expect("exists");
    assert!(
        fresh.start_date.is_empty(),
        "a todo has not started, so there is no start to record"
    );
    assert!(fresh.end_date.is_empty(), "a running project has no end");

    // An explicit start lands, and a later write leaves it alone -- unlike `updated`, which is
    // *supposed* to move.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.start_date = "2026-09-01".into();
    project::upsert(&conn, &p, None).expect("state a start");
    let started = project::get(&conn, "x").unwrap().expect("exists");
    assert_eq!(started.start_date, "2026-09-01", "an explicit start lands");

    let mut p = project_fixture("x");
    p.title = "X".into();
    p.state = "now with more prose".into();
    project::upsert(&conn, &p, None).expect("touch");
    let touched = project::get(&conn, "x").unwrap().expect("exists");
    assert_eq!(
        touched.start_date, started.start_date,
        "the start stays put"
    );
    assert!(
        touched.updated >= fresh.updated,
        "the touch timestamp moves"
    );

    // Ending it stamps the end.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "done".into();
    project::upsert(&conn, &p, None).expect("end it");
    let ended = project::get(&conn, "x").unwrap().expect("exists");
    assert!(!ended.end_date.is_empty(), "done stamps the end");
    assert!(
        ended.end_date >= ended.start_date,
        "and it sorts after the start"
    );

    // An unrelated write leaves the end where it was: this is the bug the column exists to fix.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "done".into();
    p.goal = "a footnote, months later".into();
    project::upsert(&conn, &p, None).expect("touch");
    assert_eq!(
        project::get(&conn, "x").unwrap().unwrap().end_date,
        ended.end_date,
        "`updated` moves on a touch; the end does not"
    );

    // Reopening clears it: it is not the end any more.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "active".into();
    project::upsert(&conn, &p, None).expect("reopen");
    assert!(
        project::get(&conn, "x")
            .unwrap()
            .unwrap()
            .end_date
            .is_empty(),
        "reopened = running again"
    );

    // ...and ending it again stamps a fresh one.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "aborted".into();
    project::upsert(&conn, &p, None).expect("end it again");
    assert!(
        !project::get(&conn, "x")
            .unwrap()
            .unwrap()
            .end_date
            .is_empty()
    );
}

#[test]
fn a_start_is_derived_from_leaving_todo_and_never_cleared() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    // Born a todo: nothing has started, so there is nothing to record.
    let mut p = project_fixture("x");
    p.title = "X".into();
    project::upsert(&conn, &p, None).expect("create");
    assert!(
        project::get(&conn, "x")
            .unwrap()
            .unwrap()
            .start_date
            .is_empty(),
        "a todo project has not started"
    );

    // Leaving `todo` IS the event — work began, and the store now knows when.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "active".into();
    project::upsert(&conn, &p, None).expect("activate");
    let started = project::get(&conn, "x").unwrap().unwrap();
    assert!(
        !started.start_date.is_empty(),
        "leaving todo stamps the start"
    );

    // ...and it is never cleared. Pushing the project back to `todo` keeps the date, which is the
    // record that someone worked on it and parked it.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "todo".into();
    project::upsert(&conn, &p, None).expect("park");
    assert_eq!(
        project::get(&conn, "x").unwrap().unwrap().start_date,
        started.start_date,
        "a start survives a return to todo — it is the hint that the project was dicked with"
    );

    // A project born already started gets the same fact from the same rule.
    let mut p = project_fixture("y");
    p.title = "Y".into();
    p.status = "active".into();
    project::upsert(&conn, &p, None).expect("create active");
    assert!(
        !project::get(&conn, "y")
            .unwrap()
            .unwrap()
            .start_date
            .is_empty(),
        "a project born active has started"
    );
}

#[test]
fn a_date_can_be_backfilled_but_not_invented() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    // Backfill: the work started in September and was written down in October.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "done".into();
    p.start_date = "2026-09-01".into();
    p.end_date = "2026-09-30".into();
    let key = project::upsert(&conn, &p, None).expect("backfill");
    let got = project::get(&conn, &key).unwrap().expect("exists");
    assert_eq!(got.start_date, "2026-09-01", "an explicit start wins");
    assert_eq!(
        got.end_date, "2026-09-30",
        "an explicit end is kept as given"
    );

    // You cannot date the end of something that has not ended: that is a contradiction, not a filter.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "active".into();
    p.end_date = "2026-10-15".into();
    let err = project::upsert(&conn, &p, None).unwrap_err();
    assert!(format!("{err}").contains("end_date"), "{err}");
    assert_eq!(
        project::get(&conn, "x").unwrap().unwrap().end_date,
        "2026-09-30",
        "the rejected write changed nothing"
    );

    // Nor a date that is not a date: it would sort wrongly forever.
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.start_date = "Sept 3".into();
    let err = project::upsert(&conn, &p, None).unwrap_err();
    assert!(format!("{err}").contains("ISO date"), "{err}");
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
        project::get(&conn, "x").unwrap().is_none(),
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

    let got = project::get(&conn, "x").unwrap().expect("exists");
    assert_eq!(
        got.owner,
        vec!["Cline".to_string(), "James".to_string()],
        "sorted, in the spelling it was given"
    );
    let raw: String = conn
        .query_row("SELECT owner FROM project WHERE key = 'x'", [], |r| {
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
    assert_eq!(first, "deep-work");
    assert_eq!(second, "deep-work-2", "collision gets a suffix");
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
fn a_project_tag_is_how_memories_join() {
    use ellmephant::search::{MatchMode, Query};

    // Membership is a TAG, not a relation: `prj:<key>` names the project, nothing enforces a single
    // home (a fact may bear on two projects), and nothing can dangle. A project's memories are
    // therefore just the rows carrying its tag.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed_project(&conn, "chronos");

    let mut m = memory("a", "fact");
    m.tags = vec!["prj:chronos".into(), "storage".into()];
    store::add(&conn, &embedder, &m).expect("add");

    let hits = |tags: &[&str]| -> Vec<String> {
        let q = Query {
            text: None,
            pattern: None,
            mode: MatchMode::Literal,
            k: 10,
            max_distance: None,
            expand: 0,
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
            min_chars: None,
            max_chars: None,
            coherent: None,
        };
        ellmephant::search::run(&conn, None, &q)
            .expect("search")
            .hits
            .iter()
            .map(|h| h.key.clone())
            .collect()
    };
    assert_eq!(
        hits(&["prj:chronos"]),
        vec!["a"],
        "the project tag finds it"
    );
    assert_eq!(
        hits(&["prj:chronos", "storage"]),
        vec!["a"],
        "intersected with a topic tag, still found"
    );
    assert!(
        hits(&["prj:chronos", "tax"]).is_empty(),
        "a topic tag it does not carry excludes it"
    );
    assert!(
        hits(&["prj: seaweedfs"]).is_empty(),
        "filters fold whitespace like writes do -- an unfolded spelling is a name nobody wrote"
    );
}

#[test]
fn list_filters_by_owner() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let mut p = project_fixture("ours");
    p.title = "Ours".into();
    p.owner = vec!["James".into(), "Cline".into()];
    project::upsert(&conn, &p, None).expect("upsert");

    let mut p = project_fixture("theirs");
    p.title = "Theirs".into();
    p.owner = vec!["James".into()];
    project::upsert(&conn, &p, None).expect("upsert");

    let by = |owners: &[&str]| -> Vec<String> {
        let filter = project::Filter {
            owner: owners.iter().map(|o| (*o).to_string()).collect(),
            ..Default::default()
        };
        project::list(&conn, &filter)
            .expect("list")
            .iter()
            .map(|p| p.key.clone())
            .collect()
    };
    assert_eq!(by(&["Cline"]), vec!["ours"], "only one project lists Cline");
    assert_eq!(by(&["james"]), vec!["ours", "theirs"], "case-insensitive");
    assert_eq!(by(&["Cline", "James"]), vec!["ours"], "names intersect");
    assert_eq!(
        by(&["nobody"]),
        Vec::<String>::new(),
        "an unknown name matches none"
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
        &mk("alpha", "active", vec!["continuity"], "ship the spine"),
        None,
    )
    .unwrap();
    project::upsert(
        &conn,
        &mk(
            "beta",
            "paused",
            vec!["continuity", "storage"],
            "store the facts",
        ),
        None,
    )
    .unwrap();
    project::upsert(
        &conn,
        &mk("gamma", "active", vec!["storage"], "store more"),
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
    assert_eq!(keys, vec!["alpha", "gamma"], "key order");

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
    assert_eq!(both[0].key, "beta");

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
    assert_eq!(literal[0].key, "alpha");

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
    assert_eq!(regex[0].key, "beta");
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
fn a_key_that_is_only_a_marker_is_rejected() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    for only in ["prj:", "prj-", "  prj:  "] {
        let mut p = project_fixture("x");
        p.key = only.into();
        let err = project::upsert(&conn, &p, None).unwrap_err();
        assert!(
            format!("{err}").contains("empty project key"),
            "{only:?}: {err}"
        );
    }
}

#[test]
fn preload_refuses_a_project_marker() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    // `mem_preload` holds MEMORY keys; a `prj:` token names the other kind, so say so rather than
    // write a key that can never resolve.
    let mut p = project_fixture("x");
    p.title = "X".into();
    let err = project::upsert(&conn, &p, Some(&["prj:chronos".to_string()])).unwrap_err();
    assert!(format!("{err}").contains("preload"), "{err}");

    // A bare name is a memory of that name — even one that reads like the legacy prefix, because in
    // prose (and in this list) `prj-` is just letters.
    project::upsert(&conn, &p, Some(&["prj-notes".to_string()])).expect("bare name is a memory");
    assert_eq!(project::preload(&conn, "x").unwrap()[0].key, "prj-notes");
}

#[test]
fn a_memory_and_a_project_may_share_a_name() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    // The case that moved the type out of the name: `sessions` is a project, and `sessions` is also
    // what the memories about it get called.
    project::upsert(&conn, &project_fixture("sessions"), None).expect("project");
    store::add(
        &conn,
        &embedder,
        &memory(
            "sessions",
            "see [[prj:sessions]] for the plan; [[ghost]] is rot",
        ),
    )
    .expect("memory");

    // Two rows, two tables, one name — and both are findable by it.
    assert!(project::get(&conn, "sessions").unwrap().is_some());
    assert!(store::get(&conn, "sessions").unwrap().is_some());

    let edges: Vec<(String, String)> = conn
        .prepare("SELECT to_key, to_type FROM memory_link ORDER BY to_key")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        edges,
        vec![
            ("ghost".to_string(), "memory".to_string()),
            ("sessions".to_string(), "project".to_string())
        ],
        "the marker is what crosses to the other table"
    );

    // Rot says WHICH kind is missing: `ghost` is a memory that was never written.
    let rot = links::dangling(&conn).unwrap();
    assert_eq!(rot.len(), 1, "{rot:?}");
    assert_eq!(rot[0].to_key, "ghost");
    assert_eq!(rot[0].to_type, links::Kind::Memory);
}

#[test]
fn a_project_link_is_not_a_memory_backlink() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    project::upsert(&conn, &project_fixture("sessions"), None).expect("project");
    store::add(&conn, &embedder, &memory("about", "[[prj:sessions]]")).expect("pointer");
    store::add(
        &conn,
        &embedder,
        &memory("sessions", "the memory of the same name"),
    )
    .expect("memory");

    // The edge is aimed at the PROJECT, so a memory called `sessions` has no backlinks from it...
    assert!(links::backlinks(&conn, "sessions").unwrap().is_empty());

    // ...and renaming that memory must not drag the project's edge along.
    links::rename(&conn, "sessions", "session-notes").expect("rename");
    let typed: Vec<(String, String)> = conn
        .prepare("SELECT to_key, to_type FROM memory_link")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        typed,
        vec![("sessions".to_string(), "project".to_string())],
        "the project edge outlived the memory rename"
    );
}

#[test]
fn the_wire_shapes_are_positional_and_self_describing() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed_memory(&conn, &embedder, "a");
    let key = seed_project(&conn, "chronos");
    project::upsert(&conn, &project_fixture("chronos"), Some(&["a".to_string()])).unwrap();

    let list = project::list(&conn, &project::Filter::default()).unwrap();
    let wire = project::list_to_wire_json(&list).unwrap();
    let v: serde_json::Value = serde_json::from_str(&wire).expect("valid json");
    assert_eq!(
        v["cols"],
        serde_json::json!([
            "key",
            "status",
            "owner",
            "tags",
            "title",
            "goal",
            "start_date",
            "end_date"
        ])
    );
    assert_eq!(v["projects"][0][0], serde_json::json!("chronos"));
    assert!(!wire.contains("\"key\":"), "no per-row keys: {wire}");

    let view = project::view(&conn, &key).unwrap().unwrap();
    let wire = project::view_to_wire_json(&view).unwrap();
    let v: serde_json::Value = serde_json::from_str(&wire).expect("valid json");
    assert_eq!(v["project"]["key"], serde_json::json!("chronos"));
    assert_eq!(v["preload"]["cols"], serde_json::json!(["key", "title"]));
    assert_eq!(v["preload"]["rows"][0][0], serde_json::json!("a"));
    assert!(
        v.get("contains").is_none(),
        "there is no `contains`: membership is a tag now"
    );
}

#[test]
fn merge_patches_one_field_and_leaves_the_rest_alone() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");

    let mut p = project_fixture("x");
    p.title = "X — a title".into();
    p.goal = "the goal".into();
    p.state = "the state".into();
    p.tags = vec!["storage".into()];
    project::upsert(&conn, &p, None).expect("upsert");

    // Only the goal: every other field is left exactly as it was.
    let out = project::merge(
        &conn,
        &project::Patch {
            key: Some("x".into()),
            goal: Some("a better goal".into()),
            ..Default::default()
        },
    )
    .expect("merge goal");
    assert!(!out.created, "an existing key is patched, not created");
    assert_eq!(out.key, "x");

    let got = project::get(&conn, "x").unwrap().expect("exists");
    assert_eq!(got.goal, "a better goal");
    assert_eq!(got.title, "X — a title", "the other fields are untouched");
    assert_eq!(got.state, "the state");
    assert_eq!(got.tags, vec!["storage".to_string()]);
}

#[test]
fn merge_keeps_the_status_when_it_is_not_the_field_being_written() {
    // The footgun this design removes: an omitted `status` must not silently mean `todo`.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "active".into();
    project::upsert(&conn, &p, None).expect("upsert");

    project::merge(
        &conn,
        &project::Patch {
            key: Some("x".into()),
            state: Some("where it stands".into()),
            ..Default::default()
        },
    )
    .expect("merge state");

    let got = project::get(&conn, "x").unwrap().unwrap();
    assert_eq!(
        got.status, "active",
        "a state edit must not un-start a project"
    );
    assert_eq!(got.state, "where it stands");
}

#[test]
fn merge_clears_only_where_empty_is_a_fact() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.goal = "a goal".into();
    p.state = "a state".into();
    p.owner = vec!["Cline".into()];
    p.tags = vec!["storage".into()];
    project::upsert(&conn, &p, None).expect("upsert");

    project::merge(
        &conn,
        &project::Patch {
            key: Some("x".into()),
            goal: Some(String::new()),
            state: Some(String::new()),
            owner: Some(vec![]),
            tags: Some(vec![]),
            ..Default::default()
        },
    )
    .expect("merge clears");

    let got = project::get(&conn, "x").unwrap().unwrap();
    assert_eq!(got.goal, "", "an empty goal is a fact");
    assert_eq!(got.state, "");
    assert!(got.owner.is_empty(), "an empty owner set is unassigned");
    assert!(got.tags.is_empty());
}

#[test]
fn merge_status_follows_the_project_date_rules() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    seed_project(&conn, "x");

    let status = |s: &str| project::Patch {
        key: Some("x".into()),
        status: Some(s.into()),
        ..Default::default()
    };

    // todo -> active stamps the start and leaves the end empty.
    project::merge(&conn, &status("active")).expect("activate");
    let started = project::get(&conn, "x").unwrap().unwrap();
    assert!(!started.start_date.is_empty(), "work started");
    assert!(started.end_date.is_empty());

    // active -> done stamps the end.
    project::merge(&conn, &status("done")).expect("finish");
    let done = project::get(&conn, "x").unwrap().unwrap();
    assert_eq!(done.status, "done");
    assert!(!done.end_date.is_empty(), "`done` ends it");

    // A merge that touches something else disturbs neither date.
    project::merge(
        &conn,
        &project::Patch {
            key: Some("x".into()),
            goal: Some("still done".into()),
            ..Default::default()
        },
    )
    .expect("bare patch");
    let kept = project::get(&conn, "x").unwrap().unwrap();
    assert_eq!(
        kept.end_date, done.end_date,
        "an unrelated write keeps the end"
    );
    assert_eq!(kept.start_date, done.start_date);

    // done -> active clears the end and KEEPS the start (never cleared).
    project::merge(&conn, &status("active")).expect("reopen");
    let reopened = project::get(&conn, "x").unwrap().unwrap();
    assert!(reopened.end_date.is_empty(), "reopening clears the end");
    assert_eq!(
        reopened.start_date, done.start_date,
        "the start is a past fact and is never cleared"
    );
}

#[test]
fn merge_refuses_an_empty_status_and_needs_a_title_to_create() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    seed_project(&conn, "x");

    // Empty is not an instruction here: it must not quietly read as `todo`.
    let empty = project::Patch {
        key: Some("x".into()),
        status: Some(String::new()),
        ..Default::default()
    };
    let err = project::merge(&conn, &empty).unwrap_err();
    assert!(
        format!("{err}").contains("`status` cannot be empty"),
        "{err}"
    );
    assert_eq!(project::get(&conn, "x").unwrap().unwrap().status, "todo");

    // An unknown status is still an error.
    let bad = project::Patch {
        key: Some("x".into()),
        status: Some("nope".into()),
        ..Default::default()
    };
    assert!(format!("{}", project::merge(&conn, &bad).unwrap_err()).contains("unknown status"));

    // A create keeps the old minimum: you cannot make a nameless project.
    let nameless = project::Patch {
        key: Some("ghost".into()),
        goal: Some("g".into()),
        ..Default::default()
    };
    let err = project::merge(&conn, &nameless).unwrap_err();
    assert!(format!("{err}").contains("needs a `title`"), "{err}");
    assert!(
        project::get(&conn, "ghost").unwrap().is_none(),
        "nothing was written"
    );
}

#[test]
fn merge_preload_replaces_the_list_and_keeps_it_when_absent() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    seed_project(&conn, "x");
    seed_memory(&conn, &embedder, "boot-note");
    seed_memory(&conn, &embedder, "rules");

    project::merge(
        &conn,
        &project::Patch {
            key: Some("x".into()),
            preload: Some(vec!["rules".into(), "boot-note".into()]),
            ..Default::default()
        },
    )
    .expect("set preload");
    let keys = |c: &Connection| -> Vec<String> {
        project::preload(c, "x")
            .unwrap()
            .into_iter()
            .map(|r| r.key)
            .collect()
    };
    assert_eq!(
        keys(&conn),
        vec!["rules", "boot-note"],
        "order is the payload"
    );

    // Absent: the ordered list is untouched (this was already the rule; now it is the rule for
    // every field).
    project::merge(
        &conn,
        &project::Patch {
            key: Some("x".into()),
            goal: Some("g".into()),
            ..Default::default()
        },
    )
    .expect("merge goal");
    assert_eq!(keys(&conn), vec!["rules", "boot-note"]);

    // `Some(vec![])` clears it.
    project::merge(
        &conn,
        &project::Patch {
            key: Some("x".into()),
            preload: Some(vec![]),
            ..Default::default()
        },
    )
    .expect("clear preload");
    assert!(keys(&conn).is_empty());
}

#[test]
fn merge_reports_a_create_so_a_mistyped_key_is_loud() {
    // `key` is an ADDRESS, not a field: a patch naming a different key addresses a DIFFERENT row.
    // That is a create, and `created` is the only thing that tells the caller it happened — without
    // it, "I meant to edit `old`" silently leaves a duplicate behind.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let mut p = project_fixture("old");
    p.title = "Old".into();
    project::upsert(&conn, &p, None).expect("upsert");

    let out = project::merge(
        &conn,
        &project::Patch {
            key: Some("odl".into()),
            title: Some("Old".into()),
            ..Default::default()
        },
    )
    .expect("merge");

    assert!(out.created, "a key change is a create, never a rename");
    assert_eq!(out.key, "odl");
    assert_eq!(
        project::count(&conn).unwrap(),
        2,
        "the original survives: nothing renamed it"
    );
    assert!(project::get(&conn, "old").unwrap().is_some());
}

#[test]
fn merge_refuses_an_end_date_on_a_project_that_has_not_ended() {
    // A live project may not carry an end date — `upsert` says so. The merge must say so too, or a
    // caller's `end_date` silently vanishes.
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let mut p = project_fixture("x");
    p.title = "X".into();
    p.status = "active".into();
    project::upsert(&conn, &p, None).expect("upsert");

    let err = project::merge(
        &conn,
        &project::Patch {
            key: Some("x".into()),
            end_date: Some("2026-01-01".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(format!("{err}").contains("has not ended"), "{err}");
    assert!(
        project::get(&conn, "x")
            .unwrap()
            .unwrap()
            .end_date
            .is_empty(),
        "nothing was written"
    );
}
