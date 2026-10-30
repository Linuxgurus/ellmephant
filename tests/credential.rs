//! Credentials are rows: mint issues one, rotation replaces it, and authority stays downward.
//!
//! Provisioning is in the test, not the door: a parent row is written directly, then the runtime
//! verbs are exercised exactly as the MCP tools call them.

use ellmephant::config::Paths;
use ellmephant::store::{self, credential};

fn store() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path());
    {
        let conn = ellmephant::db::open(&paths).unwrap();
        conn.execute_batch(
            "INSERT INTO pen (name, kind) VALUES ('James', 'self');
             INSERT INTO pen (name, kind) VALUES ('stet', 'self');",
        )
        .unwrap();
    }
    (dir, paths)
}

#[test]
fn mint_issues_a_secret_and_the_child_proves_with_it() {
    let (_dir, paths) = store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let minted = store::mint(&conn, "James", "cline", "self").unwrap();
    assert_eq!(minted.parent, "James");
    assert_eq!(minted.secret.len(), 64, "32 bytes, hex");
    assert_eq!(
        credential::verify(&conn, "cline", &minted.secret).unwrap(),
        Some("cline".to_string()),
        "the returned secret is the child's proof"
    );
    assert_eq!(
        credential::verify(&conn, "cline", "wrong").unwrap(),
        None,
        "a wrong proof proves nothing"
    );
}

#[test]
fn rotation_replaces_and_the_old_secret_dies_at_once() {
    let (_dir, paths) = store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let minted = store::mint(&conn, "James", "cline", "self").unwrap();
    let fresh = credential::rotate(&conn, Some("cline"), "cline").unwrap();
    assert_ne!(
        fresh, minted.secret,
        "rotation must not return the old secret"
    );
    assert_eq!(
        credential::verify(&conn, "cline", &minted.secret).unwrap(),
        None,
        "the old secret is dead on the next read (no cache)"
    );
    assert_eq!(
        credential::verify(&conn, "cline", &fresh).unwrap(),
        Some("cline".to_string())
    );
}

#[test]
fn a_parent_may_rotate_a_descendant_but_a_peer_may_not() {
    let (_dir, paths) = store();
    let conn = ellmephant::db::open(&paths).unwrap();
    store::mint(&conn, "James", "cline", "self").unwrap();
    let as_parent = credential::rotate(&conn, Some("James"), "cline");
    assert!(
        as_parent.is_ok(),
        "a parent rotates its child: {as_parent:?}"
    );
    let as_peer = credential::rotate(&conn, Some("stet"), "cline");
    assert!(
        format!("{}", as_peer.unwrap_err()).contains("never a peer"),
        "a peer may not rotate"
    );
}

#[test]
fn a_retired_pen_cannot_hold_or_use_a_credential() {
    let (_dir, paths) = store();
    let conn = ellmephant::db::open(&paths).unwrap();
    let minted = store::mint(&conn, "James", "cline", "self").unwrap();
    conn.execute(
        "UPDATE pen SET ceased_at = datetime('now') WHERE name = 'cline'",
        [],
    )
    .unwrap();
    assert_eq!(
        credential::verify(&conn, "cline", &minted.secret).unwrap(),
        None,
        "ceased_at wins over a live secret"
    );
    assert!(
        credential::rotate(&conn, None, "cline").is_err(),
        "and a tombstone is not issued a new one"
    );
}
