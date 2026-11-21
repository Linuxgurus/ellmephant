use regex::{Regex, RegexBuilder};

use crate::error::{Error, Result};
use crate::store::Memory;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    #[default]
    Literal,
    Regex,
}

#[derive(Debug)]
pub struct Matcher {
    mode: MatchMode,
    needle: String,
    re: Option<Regex>,
}

impl Matcher {
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

    pub fn matches(&self, s: &str) -> bool {
        match self.mode {
            MatchMode::Literal => s.to_lowercase().contains(&self.needle),
            MatchMode::Regex => self.re.as_ref().is_some_and(|re| re.is_match(s)),
        }
    }

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
