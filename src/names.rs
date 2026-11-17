//! NAMES THAT CARRY THEIR OWN RULES (soma/ellmephant#239).
//!
//! Two strings in this store are not interchangeable with other strings, and both have already been
//! transposed at least once: the pen that SPENDS an authority (`proven`) and the pen that TAKES the
//! credit (`claimed`), and the `from`/`into` of a merge. A `&str` cannot tell them apart; the
//! compiler can, once they are different types.
//!
//! ★ NORMALISATION IS A COMPARISON RULE, NOT A REWRITE. Pen names are stored case-PRESERVING and
//! compared case-INSENSITIVELY (migration 0027: `James` and `james` are one identity, and the stored
//! spelling wins). So [`Pen`] keeps the spelling it was given — stamping a lowercased copy would
//! rewrite authorship in the audit trail — and offers [`Pen::key`] as the comparison form. The
//! constructor is the only way in, so a `Pen` is always a name that has been through the rule.
//!
//! ★ No repository traits, no DI, no mocks (the epic's recorded non-adoption). These are values.

use crate::error::{Error, Result};

/// A pen name: non-empty, free of the one character that would break an audience list.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pen(String);

impl Pen {
    /// The one door in. A name is trimmed, and must be non-empty without a comma: an audience
    /// token is `pen:<name>` in a comma-separated list, so a comma in a name is a name that could
    /// never hold a private row — and nobody would learn that until the first private write failed
    /// (the same rule `store::mint` enforces, made a type so every caller gets it).
    pub fn new(name: &str) -> Result<Self> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Invalid("a pen needs a name".into()));
        }
        if name.contains(',') {
            return Err(Error::Invalid(format!(
                "`{name}` cannot be a pen name: a comma is the audience separator, so a pen named \
                 with one could never hold a private row"
            )));
        }
        Ok(Pen(name.to_string()))
    }

    /// The spelling the store keeps — what gets stamped.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The comparison form. Pen names are one identity across case (migration 0027).
    pub fn key(&self) -> String {
        self.0.to_ascii_lowercase()
    }

    /// Is this the same identity as `name`, whatever the case?
    pub fn is(&self, name: &str) -> bool {
        self.0.eq_ignore_ascii_case(name.trim())
    }
}

impl std::fmt::Display for Pen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Pen {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A memory key: the row's global identity and its link target (SPEC §4 #11).
///
/// Keys are `[a-z0-9-]+`. [`Key::slug`] is the creating path (text in, key out); [`Key::new`] is
/// the addressing path and REFUSES a string that is not already a key, because addressing
/// `Some Thing` and addressing `some-thing` are not the same request and silently merging them is
/// how a caller edits a row it did not name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(String);

impl Key {
    /// Address an EXISTING key. Refuses anything outside `[a-z0-9-]` — see the type docs.
    pub fn new(key: &str) -> Result<Self> {
        let key = key.trim();
        if key.is_empty() {
            return Err(Error::Invalid("a key needs a name".into()));
        }
        if !key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(Error::Invalid(format!(
                "`{key}` is not a key: keys are `[a-z0-9-]+` (use the title text and let the store \
                 slug it, or send the exact key a read returned)"
            )));
        }
        Ok(Key(key.to_string()))
    }

    /// CREATE a key from arbitrary text, the way a title becomes one.
    pub fn slug(text: &str) -> Self {
        Key(crate::store::slug::slugify(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Key {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pen_keeps_its_spelling_and_compares_across_case() {
        let pen = Pen::new("  James ").unwrap();
        assert_eq!(pen.as_str(), "James", "the stored spelling wins");
        assert_eq!(pen.key(), "james");
        assert!(pen.is("james") && pen.is("JAMES"));
        assert!(!pen.is("stet"));
    }

    #[test]
    fn a_pen_needs_a_name_and_no_comma() {
        assert!(Pen::new("   ").is_err());
        assert!(Pen::new("a,b").is_err());
    }

    #[test]
    fn a_key_refuses_text_and_slugs_when_asked() {
        assert!(Key::new("Some Thing").is_err());
        assert!(Key::new("UPPER").is_err());
        assert_eq!(Key::new("some-thing-2").unwrap().as_str(), "some-thing-2");
        assert_eq!(Key::slug("Some Thing").as_str(), "some-thing");
    }
}
