//! The word index: FTS5 over `memory(key, title, body)`, queried as BM25 (soma/ellmephant#62).
//!
//! This is the leg the store did not have. The semantic leg is unbeatable at "which document means
//! this", and useless at "which document says this word": measured on 2026-10-21, the correct memory
//! for `runner has no cargo` ranked **fifth at 0.5945** behind an unrelated one at **0.5795**. Fifteen
//! thousandths of cosine is not a ranking; it is noise wearing a number. A word index does not care.
//!
//! The table and its triggers live in migration 0017; this module only asks it questions. Keeping the
//! SQL there is deliberate — an FTS index that exists but is not maintained is worse than none, and
//! the triggers are the maintenance.

use rusqlite::Connection;

use crate::error::Result;

/// One lexical hit: the row id and its BM25 score.
///
/// ★ BM25 in SQLite is **negative and lower-is-better** (`ORDER BY rank` sorts correctly): a more
/// negative score is a stronger match. Callers must not read it as a similarity, which is why it
/// travels as `bm25` and never as `distance` — the two units are opposite and mixing them was already
/// a bug once (#59).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// `memory.id`.
    pub id: i64,
    /// SQLite's `bm25()` for this row: negative, more negative = better.
    pub bm25: f64,
}

/// The MATCH expression for a phrase, or `None` when it contains nothing indexable.
///
/// Every token is **quoted**, so FTS5 syntax in a caller's phrase cannot be executed as syntax:
/// `"NEAR("`, `"*"` and `a OR b` are all just tokens or dropped, never operators. Tokens are joined
/// with `OR` because a recall leg that required EVERY word of "how do I purge a mailbox" would return
/// nothing; ranking is BM25's job, and the semantic leg covers the queries where any-word matching is
/// too loose.
///
/// Tokens are runs of alphanumeric characters plus `_`; everything else (`-`, `.`, `/`, `:`) is a
/// separator, which is what makes `pr53-verification` find the row that contains it.
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

/// Search the index: the best `limit` rows for `phrase`, best first.
///
/// `None`/unindexable phrases and an absent table both return an empty list rather than an error:
/// a lexical leg that cannot run must not break a search that the semantic leg can still answer.
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

    /// ★ A phrase is DATA, never syntax: FTS5 operators inside it must not run.
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
