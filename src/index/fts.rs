use rusqlite::Connection;

use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub id: i64,
    pub bm25: f64,
}

pub fn match_expr(phrase: &str) -> Option<String> {
    let tokens: Vec<String> = phrase
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| t.chars().any(|c| c.is_alphanumeric()))
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect();
    if tokens.is_empty() {
        return None;
    }
    Some(tokens.join(" OR "))
}

pub fn search(conn: &Connection, phrase: &str, limit: usize) -> Result<Vec<Hit>> {
    let Some(expr) = match_expr(phrase) else {
        return Ok(Vec::new());
    };
    if !table_exists(conn)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT rowid, bm25(memory_fts) FROM memory_fts
          WHERE memory_fts MATCH ?1
          ORDER BY rank
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![expr, limit as i64], |r| {
        Ok(Hit {
            id: r.get(0)?,
            bm25: r.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn table_exists(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'memory_fts'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phrase_becomes_quoted_terms() {
        assert_eq!(
            match_expr("pr53-verification").as_deref(),
            Some("\"pr53\" OR \"verification\"")
        );
        assert_eq!(
            match_expr("how do I purge a mailbox").as_deref(),
            Some("\"how\" OR \"do\" OR \"I\" OR \"purge\" OR \"a\" OR \"mailbox\"")
        );
    }

    #[test]
    fn fts_operators_in_a_phrase_are_quoted_away() {
        assert_eq!(
            match_expr("NEAR(a b)").as_deref(),
            Some("\"NEAR\" OR \"a\" OR \"b\"")
        );
        assert_eq!(
            match_expr("a OR b*").as_deref(),
            Some("\"a\" OR \"OR\" OR \"b\"")
        );
        assert_eq!(
            match_expr("** ::: ---").as_deref(),
            None,
            "nothing indexable"
        );
        assert_eq!(match_expr("").as_deref(), None);
    }
}
