use ellmephant::config::Paths;
use ellmephant::store::credential;

#[test]
fn env_pairs_are_imported_once_and_a_deleted_credential_stays_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path());
    // SAFETY: one test binary, so nothing else reads the environment concurrently.
    unsafe {
        std::env::set_var(
            credential::BOOT_ENV,
            "import-tester:import-secret-1, import-peer:import-secret-2 ,broken,estate:root-secret",
        )
    };

    let conn = ellmephant::db::open(&paths).unwrap();
    assert_eq!(
        credential::verify(&conn, "import-tester", "import-secret-1").unwrap(),
        Some("import-tester".to_string()),
        "a name with no pen row is attached under the root and its secret hashed"
    );
    assert_eq!(
        credential::verify(&conn, "IMPORT-PEER", "import-secret-2").unwrap(),
        Some("import-peer".to_string()),
        "case folds, and the stored spelling wins"
    );
    assert_eq!(
        credential::verify(&conn, "estate", "root-secret").unwrap(),
        None,
        "the import refuses the root even when the environment names it"
    );
    let roots: i64 = conn
        .query_row(
            "SELECT count(*) FROM pen WHERE parent_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        roots, 1,
        "an env-only name is attached, never made a second root"
    );
    let parent: Option<i64> = conn
        .query_row(
            "SELECT parent_id FROM pen WHERE name = 'import-tester'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        parent.is_some(),
        "the imported pen hangs from the root, so it stays retirable"
    );

    conn.execute(
        "DELETE FROM pen_credential WHERE pen_id = (SELECT id FROM pen WHERE name = 'import-tester')",
        [],
    )
    .unwrap();
    drop(conn);

    let conn = ellmephant::db::open(&paths).unwrap();
    assert_eq!(
        credential::verify(&conn, "import-tester", "import-secret-1").unwrap(),
        None,
        "a later boot must never resurrect a revoked credential"
    );
    assert_eq!(
        credential::verify(&conn, "import-peer", "import-secret-2").unwrap(),
        Some("import-peer".to_string())
    );

    unsafe { std::env::remove_var(credential::BOOT_ENV) };
}
