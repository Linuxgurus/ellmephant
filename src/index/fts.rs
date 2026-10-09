//! FTS5 / BM25 leg (P5).
//!
//! BM25 is not optional: it carries exact tokens (hostnames, paths, identifiers) that
//! embeddings fumble (SPEC §10).

use rusqlite::params_from_iter;

use crate::error::Result;

/// A BM25 hit: rowid plus the (negative-is-better) bm25 score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FtsHit {
    /// `memory.id`.
    pub id: i64,
    /// `bm25(memory_fts)`; more negative is a better match.
    pub score: f64,
}

/// Turn free text into a safe FTS5 `MATCH` expression: each whitespace-delimited term is
/// quoted, so identifiers (`a-b.c`), `*`, `:` and stray quotes cannot produce a syntax
/// error — the whole class of "user typed punctuation and the query blew up".
///
/// Quoting also makes `infra-network-gateway` a *phrase* of its unicode61 tokens
/// (`infra network gateway`), which is what an exact-identifier lookup wants.
fn match_expr(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.replace('"', "\"\""))
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{term}\""))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

/// Rank `memory_fts` for `query`, optionally scoped, best (most negative bm25) first.
pub fn search(
    conn: &rusqlite::Connection,
    query: &str,
    scope: Option<&str>,
    limit: usize,
) -> Result<Vec<FtsHit>> {
    let Some(expr) = match_expr(query) else {
        return Ok(Vec::new());
    };
    let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);

    let (sql, args): (&str, Vec<Box<dyn rusqlite::ToSql>>) = match scope {
        Some(scope) => (
            "SELECT m.id, bm25(memory_fts) AS score
               FROM memory_fts JOIN memory m ON m.id = memory_fts.rowid
              WHERE memory_fts MATCH ?1 AND m.scope = ?2
              ORDER BY score LIMIT ?3",
            vec![Box::new(expr), Box::new(scope.to_string()), Box::new(limit)],
        ),
        None => (
            "SELECT m.id, bm25(memory_fts) AS score
               FROM memory_fts JOIN memory m ON m.id = memory_fts.rowid
              WHERE memory_fts MATCH ?1
              ORDER BY score LIMIT ?2",
            vec![Box::new(expr), Box::new(limit)],
        ),
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params_from_iter(args.iter()), |r| {
        Ok(FtsHit {
            id: r.get(0)?,
            score: r.get(1)?,
        })
    })?;
    let mut hits = Vec::new();
    for row in rows {
        hits.push(row?);
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE memory (id INTEGER PRIMARY KEY, title TEXT, body TEXT, scope TEXT);
             CREATE VIRTUAL TABLE memory_fts USING fts5(
                 title, body, content='memory', content_rowid='id',
                 tokenize='unicode61 remove_diacritics 2');
             INSERT INTO memory (id, title, body, scope) VALUES
               (1, 'Network', 'reach host portainer-notes-gw.internal', 'infra'),
               (2, 'Media',   'reach host portainer-notes-gw.internal', 'media');
             INSERT INTO memory_fts(rowid, title, body) SELECT id, title, body FROM memory;",
        )
        .expect("schema");
        conn
    }

    #[test]
    fn empty_query_is_empty() {
        assert!(search(&conn(), "   ", None, 10).unwrap().is_empty());
    }

    #[test]
    fn punctuation_does_not_break_the_query() {
        // The old failure mode: a bare `*`, `:` or `"` is FTS5 syntax.
        for q in ["a*b", "col:on", "un\"balanced", "drop-table;"] {
            assert!(search(&conn(), q, None, 10).is_ok(), "query {q:?}");
        }
    }

    #[test]
    fn an_identifier_is_a_phrase() {
        let hits = search(&conn(), "portainer-notes-gw", None, 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn scope_prefilters() {
        let hits = search(&conn(), "portainer", Some("media"), 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn limit_caps_results() {
        assert_eq!(search(&conn(), "portainer", None, 1).unwrap().len(), 1);
    }
}
