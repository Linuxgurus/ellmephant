use std::sync::Once;

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::meta;
use crate::embed::projection;
use crate::error::{Error, Result};

pub const TABLE: &str = "document_vec";

pub const DELETED_TABLE: &str = "document_vec_deleted";

const LEGACY_TABLE: &str = "memory_vec";

pub fn register_extension() {
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

#[derive(Debug, Clone, Copy)]
pub struct Knn {
    pub k: usize,
    pub max_distance: Option<f32>,
}

pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_ne_bytes()).collect()
}

pub fn from_blob(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_ne_bytes(*c))
        .collect()
}

pub fn stored_dim(conn: &Connection) -> Result<Option<usize>> {
    Ok(meta::get(conn, meta::DIM)?.and_then(|v| v.parse().ok()))
}

fn create_sql(dim: usize, table: &str) -> String {
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
    if meta::get(conn, meta::PROJECTION)?.is_none() && count(conn)? == 0 {
        meta::set(conn, meta::PROJECTION, &projection::fingerprint())?;
    }
    conn.execute_batch(&create_sql(dim, TABLE))?;
    conn.execute_batch(&create_sql(dim, DELETED_TABLE))?;
    migrate_legacy(conn)?;
    Ok(())
}

fn migrate_legacy(conn: &Connection) -> Result<()> {
    let legacy: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![LEGACY_TABLE],
        |r| r.get(0),
    )?;
    if legacy == 0 {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
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

pub fn projection_stale(conn: &Connection) -> Result<bool> {
    match meta::get(conn, meta::PROJECTION)? {
        Some(stored) => Ok(stored != projection::fingerprint()),
        None => Ok(vec_table_exists(conn)? && count(conn)? > 0),
    }
}

fn vec_table_exists(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![TABLE],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

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

pub fn upsert(conn: &Connection, id: i64, kind: &str, key: &str, vector: &[f32]) -> Result<()> {
    if vector.is_empty() {
        return Err(Error::Config("refusing to store an empty vector".into()));
    }
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

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    if !table_exists(conn)? {
        return Ok(());
    }
    conn.execute(
        &format!("DELETE FROM {DELETED_TABLE} WHERE vector_id = ?1"),
        params![id],
    )?;
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

pub fn count(conn: &Connection) -> Result<i64> {
    if !table_exists(conn)? {
        return Ok(0);
    }
    Ok(conn.query_row(&format!("SELECT count(*) FROM {TABLE}"), [], |r| r.get(0))?)
}

pub fn ids(conn: &Connection) -> Result<Vec<i64>> {
    if !table_exists(conn)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&format!("SELECT vector_id FROM {TABLE}"))?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
}

pub fn keys_of(conn: &Connection, kind: &str) -> Result<Vec<String>> {
    if !table_exists(conn)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&format!("SELECT key FROM {TABLE} WHERE type = ?1"))?;
    let rows = stmt.query_map(params![kind], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
}

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

fn table_exists(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
        params![TABLE],
        |r| r.get(0),
    )?)
}

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
        return Ok(Vec::new());
    }
    let k = i64::try_from(hint.k).map_err(|_| Error::Config("k too large".into()))?;
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

pub fn clear(conn: &Connection) -> Result<()> {
    conn.execute(&format!("DELETE FROM {TABLE}"), [])?;
    Ok(())
}

pub fn clear_all(conn: &Connection) -> Result<()> {
    conn.execute(&format!("DELETE FROM {TABLE}"), [])?;
    conn.execute(&format!("DELETE FROM {DELETED_TABLE}"), [])?;
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
        ensure(&conn, 4).unwrap();
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
        let empty = conn();
        ensure(&empty, 2).unwrap();
        empty
            .execute("DELETE FROM meta WHERE k = ?1", params![meta::PROJECTION])
            .unwrap();
        assert!(!projection_stale(&empty).unwrap());

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

        meta::set(&conn, meta::PROJECTION, "0000deadbeef").unwrap();
        assert!(projection_stale(&conn).unwrap());

        ensure(&conn, 2).unwrap();
        assert!(
            projection_stale(&conn).unwrap(),
            "ensure never overwrites a recorded fingerprint"
        );
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tombstone {
    pub kind: String,
    pub key: String,
    pub distance: f32,
    pub deleted_at: String,
}

pub fn knn_deleted(conn: &Connection, query: &[f32], hint: &Knn) -> Result<Vec<Tombstone>> {
    if query.is_empty() {
        return Err(Error::Config("empty query vector".into()));
    }
    let exists: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![DELETED_TABLE],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Ok(Vec::new());
    }
    let k = i64::try_from(hint.k).map_err(|_| Error::Config("k too large".into()))?;
    let sql = format!(
        "SELECT type, key, distance, deleted_at FROM {DELETED_TABLE} \
         WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![to_blob(query), k])?;
    let mut out: Vec<Tombstone> = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(Tombstone {
            kind: row.get(0)?,
            key: row.get(1)?,
            distance: row.get(2)?,
            deleted_at: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        });
    }
    drop(rows);
    if let Some(max) = hint.max_distance {
        out.retain(|t| t.distance <= max);
    }
    Ok(out)
}
