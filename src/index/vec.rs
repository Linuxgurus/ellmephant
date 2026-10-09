//! `sqlite-vec` encapsulation (PLAN §5 S0.1, §9).
//!
//! Everything that knows about `vec0` lives here so the extension can be swapped
//! without touching the rest of the crate. The module is registered **in-process**
//! with `sqlite3_auto_extension` — no `.dylib`, no `load_extension`.
//!
//! `memory_vec` is a *derived, rebuildable* index (like `memory_fts`), not part of the
//! relational schema: its dimension is a runtime property of the embedder, so it is
//! created and validated by [`ensure`] rather than by a SQL migration.

use std::sync::Once;

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::meta;
use crate::error::{Error, Result};

/// The `vec0` table mirroring `memory.id` (PLAN §9: swap point for the vector store).
pub const TABLE: &str = "memory_vec";

/// Register the `vec0` virtual-table module on the process. Idempotent.
///
/// Must be called *before* a connection is opened: auto-extensions only attach to
/// connections created afterwards (hence the call at the top of [`crate::db::open`]).
pub fn register_extension() -> Result<()> {
    /// The signature SQLite expects of a `sqlite3_auto_extension` entry point.
    type EntryPoint = unsafe extern "C" fn(
        *mut rusqlite::ffi::sqlite3,
        *mut *mut std::os::raw::c_char,
        *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::os::raw::c_int;

    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| unsafe {
        let init: EntryPoint = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
        rusqlite::ffi::sqlite3_auto_extension(Some(init));
    });
    Ok(())
}

/// Vector search hint.
#[derive(Debug, Clone, Copy)]
pub struct Knn<'a> {
    /// Number of neighbours.
    pub k: usize,
    /// Restrict to one scope (metadata-column filter inside the KNN scan).
    pub scope: Option<&'a str>,
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
fn create_sql(dim: usize) -> String {
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {TABLE} USING vec0(\
         memory_id INTEGER PRIMARY KEY, embedding float[{dim}], scope TEXT)"
    )
}

/// Create the `vec0` table for `dim` if missing, and reject a dimension change.
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
                "vector dim changed ({existing} -> {dim}); run `llmphant reembed` (PLAN.md P9)"
            )));
        }
        Some(_) => {}
        None => meta::set(conn, meta::DIM, &dim.to_string())?,
    }
    conn.execute_batch(&create_sql(dim))?;
    Ok(())
}

/// Drop and recreate the `vec0` table for a *new* dimension, recording it in `meta`.
///
/// This is the deliberate rebuild path [`ensure`] refuses: `reembed` (P9) calls it when the
/// model — and therefore the vector width — changes, so the frozen-dimension guard never
/// fires for an operator who means it.
pub fn recreate(conn: &Connection, dim: usize) -> Result<()> {
    if dim == 0 {
        return Err(Error::Config("embedder dim must be > 0".into()));
    }
    conn.execute_batch(&format!("DROP TABLE IF EXISTS {TABLE}"))?;
    meta::set(conn, meta::DIM, &dim.to_string())?;
    conn.execute_batch(&create_sql(dim))?;
    Ok(())
}

/// Empty the table so a full re-embed repopulates it from scratch (same dimension).
pub fn clear(conn: &Connection) -> Result<()> {
    conn.execute(&format!("DELETE FROM {TABLE}"), [])?;
    Ok(())
}

/// Insert or replace the vector for `id` (vec0 has no `ON CONFLICT`, so delete+insert).
pub fn upsert(conn: &Connection, id: i64, scope: &str, vector: &[f32]) -> Result<()> {
    if vector.is_empty() {
        return Err(Error::Config("refusing to store an empty vector".into()));
    }
    conn.execute(
        &format!("DELETE FROM {TABLE} WHERE memory_id = ?1"),
        params![id],
    )?;
    conn.execute(
        &format!("INSERT INTO {TABLE}(memory_id, embedding, scope) VALUES (?1, ?2, ?3)"),
        params![id, to_blob(vector), scope],
    )?;
    Ok(())
}

/// Remove the vector for `id` (a virtual table cannot be a FK target, so this is explicit).
pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        &format!("DELETE FROM {TABLE} WHERE memory_id = ?1"),
        params![id],
    )?;
    Ok(())
}

/// Read a stored vector back (used by tests and `doctor`).
pub fn get(conn: &Connection, id: i64) -> Result<Option<Vec<f32>>> {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            &format!("SELECT embedding FROM {TABLE} WHERE memory_id = ?1"),
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(blob.map(|b| from_blob(&b)))
}

/// Number of indexed vectors.
pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(&format!("SELECT count(*) FROM {TABLE}"), [], |r| r.get(0))?)
}

/// KNN over `memory_vec`, nearest first: `(memory_id, cosine distance)`.
pub fn knn(conn: &Connection, query: &[f32], hint: &Knn<'_>) -> Result<Vec<(i64, f32)>> {
    if query.is_empty() {
        return Err(Error::Config("empty query vector".into()));
    }
    let k = i64::try_from(hint.k).map_err(|_| Error::Config("k too large".into()))?;

    let (scope_clause, args): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match hint.scope {
        Some(s) => (
            " AND scope = ?3",
            vec![
                Box::new(to_blob(query)),
                Box::new(k),
                Box::new(s.to_string()),
            ],
        ),
        None => ("", vec![Box::new(to_blob(query)), Box::new(k)]),
    };

    let sql = format!(
        "SELECT memory_id, distance FROM {TABLE} \
         WHERE embedding MATCH ?1 AND k = ?2{scope_clause} ORDER BY distance"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, f32>(1)?))
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    if let Some(max) = hint.max_distance {
        out.retain(|(_, d)| *d <= max);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        register_extension().expect("register");
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
        upsert(&conn, 1, "infra", &[1.0, 2.0, 3.0]).unwrap();
        assert_eq!(get(&conn, 1).unwrap(), Some(vec![1.0, 2.0, 3.0]));
        upsert(&conn, 1, "infra", &[4.0, 5.0, 6.0]).unwrap();
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
        upsert(&conn, 1, "infra", &[1.0, 0.0]).unwrap();

        recreate(&conn, 4).unwrap();
        assert_eq!(stored_dim(&conn).unwrap(), Some(4));
        assert_eq!(count(&conn).unwrap(), 0, "rebuild starts empty");
        upsert(&conn, 1, "infra", &[1.0, 0.0, 0.0, 0.0]).unwrap();
        assert_eq!(get(&conn, 1).unwrap().unwrap().len(), 4);

        clear(&conn).unwrap();
        assert_eq!(count(&conn).unwrap(), 0);
    }

    #[test]
    fn knn_returns_nearest_first_and_honours_scope() {
        let conn = conn();
        ensure(&conn, 2).unwrap();
        upsert(&conn, 1, "infra", &[1.0, 0.0]).unwrap();
        upsert(&conn, 2, "infra", &[0.0, 1.0]).unwrap();
        upsert(&conn, 3, "media", &[-1.0, 0.0]).unwrap();

        let hits = knn(
            &conn,
            &[1.0, 0.0],
            &Knn {
                k: 3,
                scope: None,
                max_distance: None,
            },
        )
        .unwrap();
        assert_eq!(hits[0].0, 1, "exact match should rank first");
        assert!(hits[0].1 <= hits[1].1);

        let scoped = knn(
            &conn,
            &[1.0, 0.0],
            &Knn {
                k: 3,
                scope: Some("media"),
                max_distance: None,
            },
        )
        .unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].0, 3);
    }
}
