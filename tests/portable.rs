//! Integration: the P8 export/import round-trip against real store files.
//!
//! Runs offline — the deterministic [`HashEmbedder`] stands in for Qwen3. The sharpest
//! assertion is `export(db1) == export(db2)`: because export omits ids and vectors,
//! byte-identical JSONL proves the portable content survived the trip unchanged.

mod common;

use common::{count, temp_store};
use llmphant::db;
use llmphant::embed::HashEmbedder;
use llmphant::portable::{export, import};
use llmphant::store::{self, Memory, MemoryType};

const DIM: usize = 8;

fn memory(key: &str, title: &str, body: &str, tags: &[&str]) -> Memory {
    Memory {
        key: key.to_string(),
        scope: "infra".to_string(),
        type_: MemoryType::Signpost,
        title: title.to_string(),
        body: body.to_string(),
        source: Some("tests/portable.rs".to_string()),
        tags: tags.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn round_trip_is_byte_identical_and_idempotent() {
    let (dir1, p1) = temp_store();
    let conn1 = db::open(&p1).expect("open source");
    let embedder = HashEmbedder::new(DIM);

    store::add(
        &conn1,
        &embedder,
        &memory("alpha", "Alpha", "one [[beta]]", &["K8s"]),
    )
    .expect("add alpha");
    store::add(
        &conn1,
        &embedder,
        &memory("beta", "Beta", "two", &["infra", "k8s"]),
    )
    .expect("add beta");

    let dump = dir1.path().join("dump.jsonl");
    let written = export::to_jsonl(&conn1, &dump).expect("export");
    assert_eq!(written, 2);
    let jsonl = std::fs::read_to_string(&dump).expect("read dump");

    // Local/derived fields must not travel (SPEC §11).
    assert!(!jsonl.contains("\"id\""), "ids are local: {jsonl}");
    assert!(!jsonl.contains("\"hash\""), "hash is derived: {jsonl}");

    // Import into a fresh store.
    let (dir2, p2) = temp_store();
    let conn2 = db::open(&p2).expect("open target");
    let first = import::from_jsonl(&conn2, &embedder, &dump, false).expect("import");
    assert_eq!(first, 2, "fresh import writes every record");

    // Re-export: same bytes proves content survived minus ids/vectors.
    let dump2 = dir2.path().join("dump.jsonl");
    export::to_jsonl(&conn2, &dump2).expect("re-export");
    assert_eq!(
        jsonl,
        std::fs::read_to_string(&dump2).expect("read re-export")
    );

    // A second import of the same file changes nothing (idempotent).
    let second = import::from_jsonl(&conn2, &embedder, &dump, false).expect("re-import");
    assert_eq!(second, 0, "re-import must be a no-op");
    assert_eq!(count(&conn2, "SELECT count(*) FROM memory"), 2);
}

#[test]
fn dry_run_reports_changes_without_writing() {
    let (dir1, p1) = temp_store();
    let conn1 = db::open(&p1).expect("open source");
    let embedder = HashEmbedder::new(DIM);
    store::add(&conn1, &embedder, &memory("alpha", "Alpha", "one", &[])).expect("add");
    let dump = dir1.path().join("dump.jsonl");
    export::to_jsonl(&conn1, &dump).expect("export");

    let (_dir2, p2) = temp_store();
    let conn2 = db::open(&p2).expect("open target");
    let pending = import::from_jsonl(&conn2, &embedder, &dump, true).expect("dry-run");
    assert_eq!(pending, 1, "one record is pending");
    assert_eq!(
        count(&conn2, "SELECT count(*) FROM memory"),
        0,
        "dry run writes nothing"
    );
}

#[test]
fn changed_record_re_imports_in_place() {
    let (dir1, p1) = temp_store();
    let conn1 = db::open(&p1).expect("open source");
    let embedder = HashEmbedder::new(DIM);
    store::add(&conn1, &embedder, &memory("alpha", "Alpha", "v1", &[])).expect("add");
    let dump = dir1.path().join("dump.jsonl");
    export::to_jsonl(&conn1, &dump).expect("export");

    let (_dir2, p2) = temp_store();
    let conn2 = db::open(&p2).expect("open target");
    import::from_jsonl(&conn2, &embedder, &dump, false).expect("first import");

    // Rewrite the body in the dump, then re-import: exactly one row changes.
    let edited = std::fs::read_to_string(&dump)
        .expect("read")
        .replace("\"v1\"", "\"v2\"");
    std::fs::write(&dump, edited).expect("write");
    let changed = import::from_jsonl(&conn2, &embedder, &dump, false).expect("second import");
    assert_eq!(changed, 1, "only the edited record changes");
    assert_eq!(count(&conn2, "SELECT count(*) FROM memory"), 1);
    assert_eq!(
        store::get(&conn2, "alpha").unwrap().unwrap().body,
        "v2",
        "body was updated"
    );
}
