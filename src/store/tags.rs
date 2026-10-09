//! Tag normalisation and governance (SPEC §15 #6).
//!
//! Tags are normalised into their own table so filtering is a join and rename is a
//! single-row update.

use rusqlite::Connection;

use crate::error::Result;

/// Canonicalise a tag: trim, lowercase, collapse internal whitespace to `-`.
/// Returns `None` for tags that would be empty.
pub fn normalize(raw: &str) -> Option<String> {
    let t = raw.trim().to_lowercase().replace(char::is_whitespace, "-");
    let t = t.trim_matches('-');
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Tags no memory references, alphabetically — the vocabulary the delete path leaves behind
/// (SPEC §15 #6: the `tag` table is shared, so orphans accumulate until pruned).
pub fn orphans(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM tag WHERE id NOT IN (SELECT tag_id FROM memory_tag) ORDER BY name",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Drop every orphan tag; returns how many rows were removed.
pub fn prune(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM tag WHERE id NOT IN (SELECT tag_id FROM memory_tag)",
        [],
    )?)
}

/// Every tag with its reference count, alphabetical.
pub fn list(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT t.name, count(mt.memory_id)
           FROM tag t LEFT JOIN memory_tag mt ON mt.tag_id = t.id
          GROUP BY t.id ORDER BY t.name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_case_and_space() {
        assert_eq!(normalize("  Rust Lang ").as_deref(), Some("rust-lang"));
        assert_eq!(normalize("a  b").as_deref(), Some("a--b"));
    }

    #[test]
    fn drops_empties() {
        assert_eq!(normalize("   "), None);
        assert_eq!(normalize("--"), None);
    }
}
