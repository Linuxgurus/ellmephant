//! ORDER IS PART OF THE LAW (soma/ellmephant#238).
//!
//! Two properties, both measured rather than asserted by comment:
//!
//! 1. **Authorise before the row-reading guards.** `guard_protected` and `guard_audience` say
//!    different things for different row states, so running them before the authority check let a
//!    pen that may not write a row learn that it exists and is not public — a read disguised as a
//!    refusal.
//!
//! 2. **A patch-only refusal does not depend on the row.** Shape and contradiction read only the
//!    incoming patch, so their sentence must be byte-identical whether the key exists or not. If it
//!    is not, the refusal is a probe: "does this key exist?" answered in prose.

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

/// A store with two pens and one row owned by `james`.
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

/// ★ The refusal for a row somebody else owns must be the AUTHORITY sentence, not the protected
/// one. The protected sentence names the lease state, so hearing it would prove the row exists.
#[test]
fn authority_is_checked_before_the_protected_guard() {
    let (_dir, paths, _embedder) = fixture();

    // The observer may not write james's row, AND this patch would make it protected with no lease
    // held — two refusals are available. The one that comes back must be about authority.
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

/// ★ THE REFUSAL CANNOT DESCRIBE THE ROW (Stet's property, #238): given a caller who may not write,
/// the response must not change with the row's state — otherwise the sentence is an existence probe
/// in prose, however carefully the check itself is ordered. The SAME key is refused twice: once as a
/// plain row, then as a PROTECTED row with a live lease held by someone else. The two sentences must
/// be byte-identical, which is what "authorise before the row-reading rules" buys and what merely
/// seeing a 403 does not prove.
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
        // Make the same row PROTECTED and lease it to its owner: now a second refusal (no lease for
        // the observer) is available to any guard that runs before the authority check.
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

/// ★ A patch-only refusal is a measurement of the PATCH, so the row's existence cannot change it.
#[test]
fn a_patch_only_refusal_is_identical_for_an_existing_and_a_missing_key() {
    let (_dir, paths, _embedder) = fixture();

    // `private: true` alongside an explicit audience contradicts itself, and that is decided by the
    // patch alone — so the same sentence must come back for a key that exists and one that does not.
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

    // NOTE: the shape gate is deliberately NOT compared this way. Its refusal NAMES the key it
    // inferred (#217: "record append source=…"), so two different keys are two different sentences
    // by design; what makes it patch-only is that the row's state never enters the message.
}
