mod common;

use common::temp_store;
use ellmephant::db;
use ellmephant::embed::HashEmbedder;
use ellmephant::store::{self, Memory, Patch};

const DIM: usize = 8;

fn pen(conn: &rusqlite::Connection, name: &str, parent: Option<&str>) {
    conn.execute(
        "INSERT OR IGNORE INTO pen (name, kind, parent_id)
         VALUES (?1, 'self', (SELECT id FROM pen WHERE name = ?2))",
        rusqlite::params![name, parent],
    )
    .expect("seed pen");
}

fn fixture() -> (tempfile::TempDir, ellmephant::config::Paths, HashEmbedder) {
    let (dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    pen(&conn, "cline", None);
    pen(&conn, "james", Some("cline"));
    pen(&conn, "observer", Some("cline"));
    let embedder = HashEmbedder::new(DIM);
    let mem = Memory {
        key: "owned".to_string(),
        title: "Owned".to_string(),
        body: "james wrote this".to_string(),
        asserted_by: Some("james".to_string()),
        owner: Some("james".to_string()),
        ..Default::default()
    };
    common::add_now(&conn, &embedder, &mem).expect("seed memory");
    (dir, paths, embedder)
}

fn write(paths: &ellmephant::config::Paths, patch: Patch) -> Result<store::MergeOutcome, String> {
    let conn = db::open(paths).expect("open");
    store::merge(&conn, &patch).map_err(|e| e.to_string())
}

#[test]
fn authority_is_checked_before_the_protected_guard() {
    let (_dir, paths, _embedder) = fixture();

    let refused = write(
        &paths,
        Patch {
            key: Some("owned".into()),
            tags: Some(vec!["protected".into()]),
            acted_by: Some("observer".into()),
            asserted_by: Some("observer".into()),
            ..Default::default()
        },
    )
    .expect_err("an observer may not write james's row");

    assert!(
        !refused.contains("PROTECTED"),
        "the protected guard ran BEFORE the authority check and leaked the row's state: {refused}"
    );
    assert!(
        refused.contains("observer"),
        "the refusal should name the pen that may not write: {refused}"
    );
}

#[test]
fn the_authority_refusal_does_not_change_with_the_row_state() {
    let (_dir, paths, _embedder) = fixture();

    let patch = Patch {
        key: Some("owned".into()),
        body: Some("an observer's words".into()),
        acted_by: Some("observer".into()),
        asserted_by: Some("observer".into()),
        ..Default::default()
    };
    let plain = write(&paths, patch.clone()).expect_err("observer may not write james's row");

    {
        let conn = db::open(&paths).expect("open");
        conn.execute(
            "UPDATE memory SET tags = ?1 WHERE key = 'owned'",
            [store::tags::encode(&[store::PROTECTED.to_string()])],
        )
        .expect("protect the row");
        store::claim::acquire(&conn, "owned", "james", Some(600)).expect("james holds the lease");
    }

    let protected = write(&paths, patch).expect_err("observer may not write james's row");
    assert_eq!(
        plain, protected,
        "the refusal changed with the row's PROTECTED/leased state — the authority check is not \
         before the row-reading rules, and the refusal has become a read"
    );
}

#[test]
fn a_patch_only_refusal_is_identical_for_an_existing_and_a_missing_key() {
    let (_dir, paths, _embedder) = fixture();

    let contradicting = |key: &str| Patch {
        key: Some(key.into()),
        title: Some("T".into()),
        body: Some("B".into()),
        private: Some(true),
        audience: Some("pen:cline".into()),
        acted_by: Some("james".into()),
        asserted_by: Some("james".into()),
        ..Default::default()
    };

    let existing = write(&paths, contradicting("owned")).expect_err("contradiction is refused");
    let missing = write(&paths, contradicting("absent")).expect_err("contradiction is refused");
    assert_eq!(
        existing, missing,
        "a patch-only refusal changed with the row's existence — that is a probe, not a refusal"
    );
}
