//! The lexical leg: literal/regex matching over the raw stored text.
//!
//! Semantic search reaches *meaning*; it is useless when you half-remember a literal — a key, a
//! filename, an error string, an exact phrase you wrote. This module scans the *literal* `key`,
//! `title` and `body` columns (the stored text, *not* the link-stripped embedding projection)
//! and answers a plain boolean per row; [`run`](super::run) intersects that set with the rest of
//! the query.
//!
//! No FTS index and no `LIKE`: `memory` is the only source of truth (SPEC §5/§7), so there is
//! nothing to keep in sync — a scan over a store of thousands is cheap. Matching is
//! case-insensitive in both modes.

use regex::{Regex, RegexBuilder};

use crate::error::{Error, Result};
use crate::store::Memory;

/// How a [`Query`](super::Query)'s `pattern` is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    /// A plain substring, case-insensitive; the regex metacharacters are literal text.
    #[default]
    Literal,
    /// A [`regex`] pattern, case-insensitive by default (opt out with a leading `(?-i)`).
    Regex,
}

/// A compiled lexical predicate over a memory's `key`, `title` and `body`.
///
/// Constructed once per query (the scan compiles nothing per row), then [`hits`](Self::hits) is
/// asked of every row. Tags are deliberately *not* searched: a tag match is a structured filter
/// (`tags`), which this leg intersects with.
#[derive(Debug)]
pub struct Matcher {
    mode: MatchMode,
    /// The lowercased needle (`Literal`) or the compiled pattern (`Regex`).
    needle: String,
    re: Option<Regex>,
}

impl Matcher {
    /// Compile `pattern` under `mode`. A malformed regex is [`Error::Pattern`], surfaced before
    /// any row is scanned.
    pub fn new(pattern: &str, mode: MatchMode) -> Result<Self> {
        match mode {
            MatchMode::Literal => Ok(Self {
                mode,
                needle: pattern.to_lowercase(),
                re: None,
            }),
            MatchMode::Regex => {
                let re = RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| Error::Pattern(e.to_string()))?;
                Ok(Self {
                    mode,
                    needle: String::new(),
                    re: Some(re),
                })
            }
        }
    }

    /// Whether `s` matches the pattern. The one predicate: [`hits`](Self::hits) is this over a
    /// memory's three fields, and non-memory rows (a project's `key`/`title`/`goal`/`state`) call
    /// it directly, so the lexical leg is not duplicated per table.
    pub fn matches(&self, s: &str) -> bool {
        match self.mode {
            MatchMode::Literal => s.to_lowercase().contains(&self.needle),
            MatchMode::Regex => self.re.as_ref().is_some_and(|re| re.is_match(s)),
        }
    }

    /// Whether `mem`'s `key`, `title` or `body` contains the pattern.
    pub fn hits(&self, mem: &Memory) -> bool {
        [&mem.key, &mem.title, &mem.body]
            .into_iter()
            .any(|field| self.matches(field))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(key: &str, title: &str, body: &str) -> Memory {
        Memory {
            key: key.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn literal_is_a_case_insensitive_substring_over_key_title_body() {
        let m = Matcher::new("LOCALHOST:8888", MatchMode::Literal).unwrap();
        assert!(m.hits(&mem("k", "t", "bound to localhost:8888")));
        assert!(m.hits(&mem("k", "localhost:8888 notes", "b")));
        assert!(m.hits(&mem("localhost:8888", "t", "b")));
        assert!(!m.hits(&mem("k", "t", "nothing here")));
    }

    #[test]
    fn literal_keeps_regex_metacharacters() {
        // A literal `.` must match a dot, not any char, and `**` is two stars.
        let m = Matcher::new("a.b", MatchMode::Literal).unwrap();
        assert!(m.hits(&mem("k", "t", "a.b")));
        assert!(!m.hits(&mem("k", "t", "axb")));
    }

    #[test]
    fn regex_matches_and_is_case_insensitive_by_default() {
        let m = Matcher::new(r"err(or)?\d{3}", MatchMode::Regex).unwrap();
        assert!(m.hits(&mem("k", "t", "got ERROR500 once")));
        assert!(!m.hits(&mem("k", "t", "clean")));
    }

    #[test]
    fn regex_can_opt_into_case_sensitivity() {
        let m = Matcher::new(r"(?-i)^Deploy", MatchMode::Regex).unwrap();
        assert!(m.hits(&mem("k", "Deploy notes", "b")));
        assert!(!m.hits(&mem("k", "deploy notes", "b")));
    }

    #[test]
    fn a_bad_regex_is_a_pattern_error() {
        let err = Matcher::new("(unclosed", MatchMode::Regex).unwrap_err();
        assert!(matches!(err, Error::Pattern(_)), "{err}");
    }
}
