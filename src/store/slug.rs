//! Slug generation for the `key` column: global-unique identity *and* link target
//! (SPEC §4 #11). Key charset is `[a-z0-9-]+` so the link regex stays simple.

use std::collections::HashSet;

/// Collapse `s` into `[a-z0-9-]+`: lowercase, non-alphanumerics to `-`, runs
/// collapsed, ends trimmed. Returns `"untitled"` if nothing survives.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "untitled".to_string()
    } else {
        out
    }
}

/// First free variant of `base`, appending `-2`, `-3`, ... on collision.
pub fn unique_slug(base: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_words() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  a   b  "), "a-b");
        assert_eq!(slugify("Rust 1.99"), "rust-1-99");
    }

    #[test]
    fn empty_becomes_untitled() {
        assert_eq!(slugify("!!!"), "untitled");
        assert_eq!(slugify(""), "untitled");
    }

    #[test]
    fn collisions_get_suffixes() {
        let taken: HashSet<String> = ["note".into(), "note-2".into()].into_iter().collect();
        assert_eq!(unique_slug("note", &taken), "note-3");
        assert_eq!(unique_slug("fresh", &taken), "fresh");
    }
}
