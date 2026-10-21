//! `sqlite-vec` encapsulation (PLAN §5 S0.1, §9).
//!
//! Everything that knows about `vec0` lives here so the extension can be swapped without
//! touching the rest of the crate. The module is registered **in-process** with
//! `sqlite3_auto_extension` — no `.dylib`, no `load_extension`.
//!
//! Two vec0 tables hold the index: `document_vec` (live) and `document_vec_deleted` (tombstones).
//! Each row is one document embedding — key + title + body, as the kind rendered it — plus the
//! **partition key** `type` and the document `key` as an auxiliary column.
//!
//! Both are *derived and rebuildable*: the dimension is a runtime property of the embedder, so they
//! are created by [`ensure`] rather than by a SQL migration, and vectors can be copied between them
//! by `INSERT..SELECT` (measured: the copy answers a KNN at distance 0.0).
//!
//! ★ **`type` is a PARTITION KEY, and that is not a style choice.** Measured against the pinned
//! sqlite-vec 0.1.9: a KNN cannot filter on an auxiliary column (*"An illegal WHERE constraint was
//! provided on a vec0 auxiliary column in a KNN query"*), but it can filter on a partition key — and
//! then `k` is per-partition, which is what a type-filtered search needs. `tests/vec_kind_filter.rs`
//! holds those measurements as assertions.

use std::sync::Once;

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::meta;
use crate::embed::projection;
use crate::error::{Error, Result};

/// The live index: one vector per document, partitioned by kind.
pub const TABLE: &str = "document_vec";

/// The tombstone index: vectors of deleted documents, unreachable by any normal search.
pub const DELETED_TABLE: &str = "document_vec_deleted";

/// The table this one replaced. Vectors are COPIED out of it on first open, not re-embedded.
const LEGACY_TABLE: &str = "memory_vec";

/// Register the `vec0` virtual-table module on the process. Idempotent.
///
/// Must be called *before* a connection is opened: auto-extensions only attach to
/// connections created afterwards (hence the call at the top of [`crate::db::open`]).
pub fn register_extension() {
    /// The signature SQLite expects of a `sqlite3_auto_extension` entry point.
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

/// Vector search hint.
#[derive(Debug, Clone, Copy)]
pub struct Knn {
    /// Number of neighbours.
    pub k: usize,
    /// Optional cosine-distance ceiling.
    pub max_distance: Option<f32>,
}

/// `f32` slice -> the raw native-endian BLOB sqlite-vec reads (no header).
pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_ne_bytes()).collect()
}

/// Inverse of [`to_blob`]; a trailing partial element is ignored.
pub fn from_blob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_ne_bytes(*c))
        .collect()
}

/// The dimension recorded in `meta`, if the store has ever been written to.
pub fn stored_dim(conn: &Connection) -> Result<Option<usize>> {
    Ok(meta::get(conn, meta::DIM)?.and_then(|v| v.parse().ok()))
}

/// The `CREATE VIRTUAL TABLE` for a given dimension.
fn create_sql(dim: usize, table: &str) -> String {
    // The tombstone table carries WHEN the document went, so a deleted-search can report an age. The
    // live table has no such column: a live document has no deletion to date.
    let extra = if table == DELETED_TABLE {
        ", +deleted_at TEXT"
    } else {
        ""
    };
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING vec0(\
         vector_id INTEGER PRIMARY KEY, \
         embedding float[{dim}] distance_metric=cosine, \
         type text PARTITION KEY, \
         +key TEXT{extra})",
    )
}

/// Create the vec0 table for `dim` if missing, and reject a dimension change.
///
/// The dimension is frozen at `CREATE`: switching models is a `reembed` (P9), not a
/// silent reinterpretation of stored vectors.
pub fn ensure(conn: &Connection, dim: usize) -> Result<()> {
    if dim == 0 {
        return Err(Error::Config("embedder dim must be > 0".into()));
    }
    match stored_dim(conn)? {
        Some(existing) if existing != dim => {
            return Err(Error::Config(format!(
                "vector dim changed ({existing} -> {dim}); call the `memory_reembed` tool (PLAN.md P9)"
            )));
        }
        Some(_) => {}
        None => meta::set(conn, meta::DIM, &dim.to_string())?,
    }
    // Record the projection recipe the first time the index is touched, so a later change to what
    // `embed_document` emits is *detected* rather than silently leaving every vector describing the
    // old recipe. Only set when absent: a stored fingerprint is the baseline a mismatch is measured
    // against, and a mere open must never overwrite it.
    if meta::get(conn, meta::PROJECTION)?.is_none() {
        meta::set(conn, meta::PROJECTION, &projection::fingerprint())?;
    }
    conn.execute_batch(&create_sql(dim, TABLE))?;
    conn.execute_batch(&create_sql(dim, DELETED_TABLE))?;
    migrate_legacy(conn)?;
    Ok(())
}

/// Move the old single-kind index into the partitioned one, once.
///
/// `memory_vec` was keyed by `memory_id`; the new table is keyed by an opaque `vector_id` and
/// partitioned by `type`. Dimension and model are unchanged, so the vectors are **copied, never
/// re-embedded** — measured: an `INSERT..SELECT` copy answers a KNN at distance 0.0. The ids stay the
/// memory ids, which is why nothing has to be renumbered for the memory kind.
///
/// Idempotent by construction: it does nothing when the legacy table is absent, and drops it when it
/// has finished, so a crash mid-copy leaves the legacy table intact and the copy re-runs.
fn migrate_legacy(conn: &Connection) -> Result<()> {
    let legacy: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![LEGACY_TABLE],
        |r| r.get(0),
    )?;
    if legacy == 0 {
        return Ok(());
    }
    // ONE TRANSACTION, and the drop is the commit marker — stet's condition, and it was not met
    // before: `execute_batch` autocommits each statement, so the INSERT could land while the DROP did
    // not. That state is worse than a duplicate: `document_vec` holds the rows AND the legacy table
    // exists, re-running hits a PRIMARY KEY conflict, `ensure` returns Err, and the store will not
    // open again until someone repairs it by hand.
    let tx = conn.unchecked_transaction()?;
    // `WHERE NOT EXISTS` so a half-copied state COMPLETES rather than colliding: the copy is a
    // fact-per-row repair, not an all-or-nothing insert.
    tx.execute_batch(&format!(
        "INSERT INTO {TABLE}(vector_id, embedding, type, key)
           SELECT v.memory_id, v.embedding, 'memory', m.key
             FROM {LEGACY_TABLE} v JOIN memory m ON m.id = v.memory_id
            WHERE NOT EXISTS (SELECT 1 FROM {TABLE} d WHERE d.vector_id = v.memory_id);"
    ))?;
    tx.execute_batch(&format!("DROP TABLE {LEGACY_TABLE};"))?;
    tx.commit()?;
    tracing::info!(
        "index: copied every vector out of {LEGACY_TABLE} into {TABLE}, partitioned by type"
    );
    Ok(())
}

/// Whether the stored vectors were built with a DIFFERENT projection than the code now emits —
/// i.e. a re-embed is needed ([`crate::store::maintain::reembed`] rewrites the fingerprint).
///
/// Two ways to be stale:
///
/// - a fingerprint is recorded and it no longer matches the code, or
/// - **no** fingerprint is recorded but vectors already exist. That means the store was written
///   before projections were fingerprinted, so its vectors cannot be *proven* to match the current
///   recipe. (A store's first write records the fingerprint via [`ensure`] *before* it inserts a
///   vector, so a fresh store never lands here.) This is what makes a projection change safe to
///   ship to an old store: the drift is flagged, not assumed away.
///
/// `false` for an empty store, or one with no vectors — there is nothing to be wrong about.
pub fn projection_stale(conn: &Connection) -> Result<bool> {
    match meta::get(conn, meta::PROJECTION)? {
        Some(stored) => Ok(stored != projection::fingerprint()),
        None => Ok(vec_table_exists(conn)? && count(conn)? > 0),
    }
}

/// Whether the `vec0` table has been created yet (it appears on the first write, not on a migration).
fn vec_table_exists(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![TABLE],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Drop and recreate the table for a *new* dimension, recording it in `meta`.
///
/// This is the deliberate rebuild path [`ensure`] refuses: `reembed` (P9) calls it when the
/// model — and therefore the vector width — changes.
pub fn recreate(conn: &Connection, dim: usize) -> Result<()> {
    if dim == 0 {
        return Err(Error::Config("embedder dim must be > 0".into()));
    }
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {TABLE}; DROP TABLE IF EXISTS {DELETED_TABLE};"
    ))?;
    meta::set(conn, meta::DIM, &dim.to_string())?;
    conn.execute_batch(&create_sql(dim, TABLE))?;
    conn.execute_batch(&create_sql(dim, DELETED_TABLE))?;
    Ok(())
}

/// Insert or replace the vector for `id`, recording which document it belongs to.
///
/// The partition (`type`) and the `key` ride along so the row is self-describing: a KNN can filter by
/// type, and a document can be found without knowing its vector id.
pub fn upsert(conn: &Connection, id: i64, kind: &str, key: &str, vector: &[f32]) -> Result<()> {
    if vector.is_empty() {
        return Err(Error::Config("refusing to store an empty vector".into()));
    }
    // vec0 has no `ON CONFLICT`, so delete+insert; the savepoint keeps the pair atomic
    // even when the caller owns no transaction.
    conn.execute_batch("SAVEPOINT vec_upsert")?;
    let written = conn
        .execute(
            &format!("DELETE FROM {TABLE} WHERE vector_id = ?1"),
            params![id],
        )
        .and_then(|_| {
            conn.execute(
                &format!(
                    "INSERT INTO {TABLE}(vector_id, embedding, type, key) VALUES (?1, ?2, ?3, ?4)"
                ),
                params![id, to_blob(vector), kind, key],
            )
        });
    match written {
        Ok(_) => conn.execute_batch("RELEASE vec_upsert")?,
        Err(err) => {
            conn.execute_batch("ROLLBACK TO vec_upsert; RELEASE vec_upsert")?;
            return Err(err.into());
        }
    }
    Ok(())
}

/// Remove the vector for `id` (a virtual table cannot be a FK target, so this is explicit).
pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    if !table_exists(conn)? {
        // No table means no vector to delete — the same answer as an empty table (see `get`). This is
        // reachable now: the table appears on the first *embed*, not the first write, so deleting a
        // memory from a store that has not encoded anything yet lands here.
        return Ok(());
    }
    // A DELETE MOVES THE VECTOR. James, 2026-10-21: an embedding is semantic, not the source of
    // truth, so deleting a document should not erase what the store once knew about it. The vector
    // goes to the tombstone table, where no normal search can reach it (it is a different table, not a
    // filtered one) and a deliberate deleted-search can. A kind with a retention promise purges
    // instead — `purge` is the verb for that, never this one.
    conn.execute(
        &format!(
            "INSERT INTO {DELETED_TABLE}(vector_id, embedding, type, key, deleted_at)
               SELECT vector_id, embedding, type, key, datetime('now') FROM {TABLE} WHERE vector_id = ?1;"
        ),
        params![id],
    )?;
    conn.execute(
        &format!("DELETE FROM {TABLE} WHERE vector_id = ?1"),
        params![id],
    )?;
    Ok(())
}

/// Destroy a document's vector outright: no tombstone, no trace. For a kind whose deletion is a
/// promise to a person (`mail`), and for `memory_history_erase`.
pub fn purge(conn: &Connection, id: i64) -> Result<()> {
    if !table_exists(conn)? {
        return Ok(());
    }
    conn.execute(
        &format!("DELETE FROM {TABLE} WHERE vector_id = ?1"),
        params![id],
    )?;
    conn.execute(
        &format!("DELETE FROM {DELETED_TABLE} WHERE vector_id = ?1"),
        params![id],
    )?;
    Ok(())
}

/// Read a stored vector back (used by tests and `doctor`). `None` when the store has never been
/// written — no table means no vector, the same answer as an empty table.
pub fn get(conn: &Connection, id: i64) -> Result<Option<Vec<f32>>> {
    if !table_exists(conn)? {
        return Ok(None);
    }
    let blob: Option<Vec<u8>> = conn
        .query_row(
            &format!("SELECT embedding FROM {TABLE} WHERE vector_id = ?1"),
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(blob.map(|b| from_blob(&b)))
}

/// Number of indexed vectors. Zero for a store that has never been written: the vec0 table only
/// exists once [`ensure`] has run, and "no table" and "an empty table" mean the same thing to any
/// caller — nothing is indexed. (Without this a text search on a brand-new store answered
/// `database error: no such table: memory_vec` instead of an empty result.)
pub fn count(conn: &Connection) -> Result<i64> {
    if !table_exists(conn)? {
        return Ok(0);
    }
    Ok(conn.query_row(&format!("SELECT count(*) FROM {TABLE}"), [], |r| r.get(0))?)
}

/// Every `memory_id` that has a stored vector. Empty when the table has not been created.
///
/// A stamp saying "this row was indexed" is not the same as "this row HAS a vector": a killed
/// rebuild or a hand-deleted vector leaves the hash intact and the index torn. `reembed` uses this to
/// see the difference.
pub fn ids(conn: &Connection) -> Result<Vec<i64>> {
    if !table_exists(conn)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&format!("SELECT vector_id FROM {TABLE}"))?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
}

/// The document keys of one kind that have a live vector. `missing` compares this against the kind's
/// rows, so a document that moved kind (it cannot) or lost its vector is visible.
pub fn keys_of(conn: &Connection, kind: &str) -> Result<Vec<String>> {
    if !table_exists(conn)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&format!("SELECT key FROM {TABLE} WHERE type = ?1"))?;
    let rows = stmt.query_map(params![kind], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
}

/// Vectors held for one kind — the number `/health` and `doctor` report per type, so a single count
/// can never again stand in for "is everything indexed".
pub fn count_of(conn: &Connection, kind: &str) -> Result<i64> {
    if !table_exists(conn)? {
        return Ok(0);
    }
    Ok(conn.query_row(
        &format!("SELECT count(*) FROM {TABLE} WHERE type = ?1"),
        params![kind],
        |r| r.get(0),
    )?)
}

/// Tombstoned vectors — deleted documents, reachable only by asking for them.
pub fn count_deleted(conn: &Connection) -> Result<i64> {
    let exists: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![DELETED_TABLE],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Ok(0);
    }
    Ok(
        conn.query_row(&format!("SELECT count(*) FROM {DELETED_TABLE}"), [], |r| {
            r.get(0)
        })?,
    )
}

/// Whether the vec0 table has been created yet.
fn table_exists(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
        params![TABLE],
        |r| r.get(0),
    )?)
}

/// KNN over the table, nearest first: `(memory_id, cosine distance)`.
pub fn knn(
    conn: &Connection,
    query: &[f32],
    hint: &Knn,
    kind: Option<&str>,
) -> Result<Vec<(i64, f32)>> {
    if query.is_empty() {
        return Err(Error::Config("empty query vector".into()));
    }
    if !table_exists(conn)? {
        return Ok(Vec::new()); // nothing indexed: nothing can be near
    }
    let k = i64::try_from(hint.k).map_err(|_| Error::Config("k too large".into()))?;
    // `k` is per-partition when a type is given and global when it is not (measured). An unfiltered
    // search therefore has to be built per kind and merged, or the largest kind takes every slot.
    let sql = match kind {
        Some(_) => format!(
            "SELECT vector_id, distance FROM {TABLE} \
             WHERE embedding MATCH ?1 AND k = ?2 AND type = ?3 ORDER BY distance"
        ),
        None => format!(
            "SELECT vector_id, distance FROM {TABLE} \
             WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance"
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = match kind {
        Some(kind) => stmt.query(params![to_blob(query), k, kind])?,
        None => stmt.query(params![to_blob(query), k])?,
    };
    let mut out: Vec<(i64, f32)> = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    drop(rows);
    if let Some(max) = hint.max_distance {
        out.retain(|(_, d)| *d <= max);
    }
    Ok(out)
}

/// Empty the table so a full re-embed repopulates it from scratch (same dimension).
pub fn clear(conn: &Connection) -> Result<()> {
    conn.execute(&format!("DELETE FROM {TABLE}"), [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        register_extension();
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);")
            .expect("meta");
        conn
    }

    #[test]
    fn blob_round_trips() {
        let v = vec![0.0f32, 1.5, -2.25, 1e-3];
        assert_eq!(from_blob(&to_blob(&v)), v);
        assert_eq!(to_blob(&[0.0f32; 8]).len(), 32);
    }

    #[test]
    fn extension_registers_and_reports_a_version() {
        let conn = conn();
        let version: String = conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))
            .expect("vec_version()");
        assert!(version.starts_with('v'), "unexpected version {version:?}");
    }

    #[test]
    fn ensure_round_trips_a_vector() {
        let conn = conn();
        ensure(&conn, 3).unwrap();
        upsert(&conn, 1, "memory", "k", &[1.0, 2.0, 3.0]).unwrap();
        assert_eq!(get(&conn, 1).unwrap(), Some(vec![1.0, 2.0, 3.0]));

        upsert(&conn, 1, "memory", "k", &[4.0, 5.0, 6.0]).unwrap();
        assert_eq!(get(&conn, 1).unwrap(), Some(vec![4.0, 5.0, 6.0]));
        assert_eq!(count(&conn).unwrap(), 1);

        delete(&conn, 1).unwrap();
        assert_eq!(count(&conn).unwrap(), 0);
    }

    #[test]
    fn ensure_rejects_a_dimension_change() {
        let conn = conn();
        ensure(&conn, 4).unwrap();
        ensure(&conn, 4).unwrap(); // idempotent
        let err = ensure(&conn, 8).unwrap_err();
        assert!(format!("{err}").contains("dim changed"));
    }

    #[test]
    fn recreate_switches_dimension_and_clears() {
        let conn = conn();
        ensure(&conn, 2).unwrap();
        upsert(&conn, 1, "memory", "k", &[1.0, 0.0]).unwrap();

        recreate(&conn, 4).unwrap();
        assert_eq!(stored_dim(&conn).unwrap(), Some(4));
        assert_eq!(count(&conn).unwrap(), 0, "rebuild starts empty");
        upsert(&conn, 1, "memory", "k", &[1.0, 0.0, 0.0, 0.0]).unwrap();
        assert_eq!(get(&conn, 1).unwrap().unwrap().len(), 4);

        clear(&conn).unwrap();
        assert_eq!(count(&conn).unwrap(), 0);
    }

    #[test]
    fn from_blob_ignores_a_trailing_partial_element() {
        // 9 bytes = two whole f32 plus one stray byte; the tail must be dropped, not panic.
        let mut bytes = to_blob(&[1.0f32, 2.0]);
        bytes.push(0x7f);
        assert_eq!(from_blob(&bytes), vec![1.0f32, 2.0]);
    }

    #[test]
    fn zero_dim_is_rejected_by_ensure_and_recreate() {
        let conn = conn();
        assert!(ensure(&conn, 0).is_err());
        assert!(recreate(&conn, 0).is_err());
    }

    #[test]
    fn upsert_rejects_an_empty_vector() {
        let conn = conn();
        ensure(&conn, 2).unwrap();
        assert!(upsert(&conn, 1, "memory", "k", &[]).is_err());
    }

    #[test]
    fn knn_rejects_an_empty_query() {
        let conn = conn();
        ensure(&conn, 2).unwrap();
        let hint = Knn {
            k: 1,
            max_distance: None,
        };
        assert!(knn(&conn, &[], &hint, Some("memory")).is_err());
    }

    #[test]
    fn knn_returns_nearest_first() {
        let conn = conn();
        ensure(&conn, 2).unwrap();
        upsert(&conn, 1, "memory", "k", &[1.0, 0.0]).unwrap();
        upsert(&conn, 2, "memory", "k", &[0.0, 1.0]).unwrap();

        let hits = knn(
            &conn,
            &[1.0, 0.0],
            &Knn {
                k: 3,
                max_distance: None,
            },
            Some("memory"),
        )
        .unwrap();
        assert_eq!(hits[0].0, 1, "exact match should rank first");
        assert!(hits[0].1 <= hits[1].1);
    }

    #[test]
    fn a_vector_without_a_fingerprint_is_stale() {
        // An empty store has nothing to be wrong about.
        let empty = conn();
        ensure(&empty, 2).unwrap();
        empty
            .execute("DELETE FROM meta WHERE k = ?1", params![meta::PROJECTION])
            .unwrap();
        assert!(!projection_stale(&empty).unwrap());

        // A store written before projections were fingerprinted: vectors exist, nothing recorded —
        // so they cannot be proven to match the current projection. Flagged, not assumed away.
        let conn = conn();
        ensure(&conn, 2).unwrap();
        upsert(&conn, 1, "memory", "k", &[1.0, 0.0]).unwrap();
        conn.execute("DELETE FROM meta WHERE k = ?1", params![meta::PROJECTION])
            .unwrap();
        assert!(projection_stale(&conn).unwrap());
    }

    #[test]
    fn projection_fingerprint_is_recorded_and_a_mismatch_reads_stale() {
        let conn = conn();
        ensure(&conn, 2).unwrap();
        assert!(
            !projection_stale(&conn).unwrap(),
            "the first `ensure` records the current projection as the baseline"
        );

        // A fingerprint left by a *different* projection reads as stale...
        meta::set(&conn, meta::PROJECTION, "0000deadbeef").unwrap();
        assert!(projection_stale(&conn).unwrap());

        // ...and a later `ensure` must NOT erase it — only a re-embed may move the baseline.
        ensure(&conn, 2).unwrap();
        assert!(
            projection_stale(&conn).unwrap(),
            "ensure never overwrites a recorded fingerprint"
        );
    }
}
