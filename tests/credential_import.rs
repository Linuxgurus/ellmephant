//! The one-shot import from `ELLMEPHANT_IDENTITIES` — and the thing it must never do: resurrect.
//!
//! This lives in its own test binary because the env var is process-global, and a test that sets it
//! must not decide another test's identity.

use ellmephant::config::Paths;
use ellmephant::store::credential;

#[test]
fn env_pairs_are_imported_once_and_a_deleted_credential_stays_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path());
    // SAFETY: this test binary is one test, so nothing else reads the environment concurrently.
    unsafe {
        std::env::set_var(
            credential::BOOT_ENV,
            "import-tester:import-secret-1, import-peer:import-secret-2 ,broken",
        )
    };

    let conn = ellmephant::db::open(&paths).unwrap();
    assert_eq!(
        credential::verify(&conn, "import-tester", "import-secret-1").unwrap(),
        Some("import-tester".to_string()),
        "a name with no pen row is registered and its secret hashed"
    );
    assert_eq!(
        credential::verify(&conn, "IMPORT-PEER", "import-secret-2").unwrap(),
        Some("import-peer".to_string()),
        "case folds, and the stored spelling wins"
    );

    // Revoke by deleting the row — the way an ancestor's act would. The env still names it.
    conn.execute(
        "DELETE FROM pen_credential WHERE pen_id = (SELECT id FROM pen WHERE name = 'import-tester')",
        [],
    )
    .unwrap();
    drop(conn);

    // Re-open: the marker makes the import one-shot, so the deleted credential is NOT re-created.
    let conn = ellmephant::db::open(&paths).unwrap();
    assert_eq!(
        credential::verify(&conn, "import-tester", "import-secret-1").unwrap(),
        None,
        "a later boot must never resurrect a revoked credential"
    );
    // The other one is untouched.
    assert_eq!(
        credential::verify(&conn, "import-peer", "import-secret-2").unwrap(),
        Some("import-peer".to_string())
    );

    unsafe { std::env::remove_var(credential::BOOT_ENV) };
}
