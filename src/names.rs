use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pen(String);

impl Pen {
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

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn key(&self) -> String {
        self.0.to_ascii_lowercase()
    }

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(String);

impl Key {
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
