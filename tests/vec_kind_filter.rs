//! Probe: `vec0` 0.1.9 — can a KNN be restricted to one document type?
//!
//! Throwaway measurement for MULTI-KIND-EMBEDDINGS.md; delete when answered.
//! Result, measured 2026-10-21: auxiliary columns (`+type TEXT`) CANNOT be filtered in a KNN query
//! ("An illegal WHERE constraint was provided on a vec0 auxiliary column in a KNN query"), so the
//! filter has to be a `partition key`.

use std::sync::Once;

use rusqlite::Connection;

fn register() {
    type EntryPoint = unsafe extern "C" fn(
        *mut rusqlite::ffi::sqlite3,
        *mut *mut std::os::raw::c_char,
        *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::os::raw::c_int;
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let init =
            std::mem::transmute::<*const (), EntryPoint>(sqlite_vec::sqlite3_vec_init as *const ());
        rusqlite::ffi::sqlite3_auto_extension(Some(init));
    });
}

fn blob(v: [f32; 4]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_ne_bytes()).collect()
}

fn partition_table() -> Connection {
    register();
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch(
        "CREATE VIRTUAL TABLE v USING vec0(
           vector_id INTEGER PRIMARY KEY,
           embedding float[4] distance_metric=cosine,
           type text partition key,
           +key TEXT
         );",
    )
    .expect("create with partition key");
    for (id, e, ty, key) in [
        (1, [1.0, 0.0, 0.0, 0.0], "memory", "mem-a"),
        (2, [0.9, 0.1, 0.0, 0.0], "memory", "mem-b"),
        (3, [0.0, 1.0, 0.0, 0.0], "project", "prj-a"),
        (4, [0.1, 0.9, 0.0, 0.0], "project", "prj-b"),
    ] {
        conn.execute(
            "INSERT INTO v(vector_id, embedding, type, key) VALUES(?1, ?2, ?3, ?4)",
            rusqlite::params![id, blob(e), ty, key],
        )
        .expect("insert");
    }
    conn
}

fn knn(conn: &Connection, sql: &str, k: usize) -> Vec<(String, String, f64)> {
    let mut stmt = conn.prepare(sql).expect("prepare");
    stmt.query_map(
        rusqlite::params![blob([1.0, 0.0, 0.0, 0.0]), k as i64],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("rows")
}

#[test]
fn partition_key_filters_a_knn() {
    let conn = partition_table();
    let hits = knn(
        &conn,
        "SELECT type, key, distance FROM v WHERE embedding MATCH ? AND k = ? AND type = 'project' ORDER BY distance",
        2,
    );
    println!("PARTITIONED KNN type=project k=2: {hits:?}");
    assert_eq!(hits.len(), 2, "k is per partition, not pre-filtered");
    assert!(hits.iter().all(|(t, _, _)| t == "project"));
}

#[test]
fn partition_key_unfiltered_still_works() {
    let conn = partition_table();
    let hits = knn(
        &conn,
        "SELECT type, key, distance FROM v WHERE embedding MATCH ? AND k = ? ORDER BY distance",
        4,
    );
    println!("UNFILTERED KNN k=4: {hits:?}");
    assert_eq!(hits.len(), 4);
    assert_eq!(hits[0].1, "mem-a");
}

#[test]
fn aux_column_survives_update_and_delete() {
    let conn = partition_table();
    conn.execute(
        "UPDATE v SET embedding = ?1, key = 'moved' WHERE vector_id = 3",
        rusqlite::params![blob([0.0, 0.0, 1.0, 0.0])],
    )
    .expect("aux update via UPDATE");
    let key: String = conn
        .query_row("SELECT key FROM v WHERE vector_id = 3", [], |r| r.get(0))
        .expect("read back aux");
    assert_eq!(key, "moved");

    // Can a row move PARTITION? (a kind never changes in practice, but the row's type must be
    // writeable at insert time — check the error is honest if it cannot move.)
    let moved = conn.execute("UPDATE v SET type = 'memory' WHERE vector_id = 3", []);
    println!("MOVE PARTITION: {moved:?}");

    conn.execute("DELETE FROM v WHERE vector_id = 4", [])
        .expect("delete");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM v", [], |r| r.get(0))
        .expect("count");
    assert_eq!(n, 3);
}

#[test]
fn keyslug_is_derivable_in_sql() {
    let conn = partition_table();
    let slug: String = conn
        .query_row(
            "SELECT type || ':' || key FROM v WHERE vector_id = 3",
            [],
            |r| r.get(0),
        )
        .expect("derive keyslug");
    println!("DERIVED KEYSLUG: {slug}");
    assert_eq!(slug, "project:prj-a");
}

#[test]
fn asks_the_global_k_question() {
    let conn = partition_table();
    // Does k apply GLOBALLY when no partition is named, or per partition? The answer decides
    // whether a cross-kind search can trust `k`.
    let hits = knn(
        &conn,
        "SELECT type, key, distance FROM v WHERE embedding MATCH ? AND k = ? ORDER BY distance",
        2,
    );
    println!("UNFILTERED k=2 returned {} rows: {hits:?}", hits.len());
}

#[test]
fn vectors_can_be_copied_between_vec0_tables() {
    // The migration question: if dim and model are unchanged, can the existing vectors move into the
    // new table by INSERT..SELECT instead of being re-embedded (~0.5-1 h)?
    register();
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch(
        "CREATE VIRTUAL TABLE old_vec USING vec0(
           memory_id INTEGER PRIMARY KEY,
           embedding float[4] distance_metric=cosine
         );
         CREATE VIRTUAL TABLE new_vec USING vec0(
           vector_id INTEGER PRIMARY KEY,
           embedding float[4] distance_metric=cosine,
           type text partition key,
           +key TEXT
         );
         INSERT INTO old_vec(memory_id, embedding) VALUES(1, X'0000803F000000000000000000000000');",
    )
    .expect("seed old table");

    let copied = conn.execute(
        "INSERT INTO new_vec(vector_id, embedding, type, key)
         SELECT memory_id, embedding, 'memory', 'k' || memory_id FROM old_vec",
        [],
    );
    println!("COPY RESULT: {copied:?}");

    // If it copied, the new table must answer a KNN with the same semantics.
    if copied.is_ok() {
        let hits = knn(
            &conn,
            "SELECT type, key, distance FROM new_vec WHERE embedding MATCH ? AND k = ? AND type = 'memory' ORDER BY distance",
            4,
        );
        println!("KNN ON COPIED VECTOR: {hits:?}");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].2, 0.0, "identical vector, distance 0");
    }
}

#[test]
fn a_deleted_document_moves_to_the_tombstone_index() {
    // Deleting a document must not erase what the store knew about it: the vector moves to a sibling
    // table, so normal search stops returning it and a deleted-only search can still find it.
    register();
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch(
        "CREATE VIRTUAL TABLE live USING vec0(
           vector_id INTEGER PRIMARY KEY,
           embedding float[4] distance_metric=cosine,
           type text partition key,
           +key TEXT
         );
         CREATE VIRTUAL TABLE gone USING vec0(
           vector_id INTEGER PRIMARY KEY,
           embedding float[4] distance_metric=cosine,
           type text partition key,
           +key TEXT,
           +deleted_at TEXT
         );
         INSERT INTO live(vector_id, embedding, type, key) VALUES
           (1, X'0000803F000000000000000000000000', 'memory', 'keep'),
           (2, X'0000803F000000000000000000000000', 'memory', 'drop');",
    )
    .expect("seed live");

    // The move, as one transaction would do it.
    conn.execute_batch(
        "INSERT INTO gone(vector_id, embedding, type, key, deleted_at)
           SELECT vector_id, embedding, type, key, datetime('now') FROM live WHERE key = 'drop';
         DELETE FROM live WHERE key = 'drop';",
    )
    .expect("move to tombstone");

    let live_hits = knn(
        &conn,
        "SELECT type, key, distance FROM live WHERE embedding MATCH ? AND k = ? AND type = 'memory'",
        10,
    );
    println!("LIVE AFTER DELETE: {live_hits:?}");
    assert_eq!(live_hits.len(), 1);
    assert_eq!(live_hits[0].1, "keep");

    let mut stmt = conn
        .prepare("SELECT key, deleted_at FROM gone WHERE embedding MATCH ? AND k = ?")
        .expect("prepare tombstone knn");
    let tombstones: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![blob([1.0, 0.0, 0.0, 0.0]), 10i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    println!("TOMBSTONES: {tombstones:?}");
    assert_eq!(tombstones.len(), 1);
    assert_eq!(tombstones[0].0, "drop");
    assert!(!tombstones[0].1.is_empty(), "deleted_at is readable");
}
