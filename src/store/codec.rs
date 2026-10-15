//! The comma-joined column codec: the delimiter invariant, in one home (SPEC §9).
//!
//! Three columns are comma-joined lists — `memory.tags`, `project.mem_preload`, and
//! `project.owner` — so all three obey one rule: **a comma must not occur inside an element**, or
//! decoding splits one element into two. What differs is how each column keeps the rule:
//!
//! - `tags` is **normalised**, so it folds a comma away ([`crate::store::tags::normalize`]
//!   replaces it with `-`).
//! - `preload` holds memory keys, which are `[a-z0-9-]` after `slugify`, so a comma cannot occur.
//! - `owner` is **free text** — a name is a name, and the delimiter cannot be folded away — so a
//!   comma in one must be **refused** on write ([`crate::store::project::encode_owners`]).
//!
//! The shared half is the decode: [`split`] is the one splitter all three columns use, so an
//! element is always trimmed to its name and the three cannot drift apart. (The encoders are
//! deliberately *not* shared: they differ on the axes that matter — normalise, dedup key, sort,
//! and whether they can fail — so a generic `encode_list` would be indirection, not clarity.)

/// The delimiter every comma-joined column uses.
pub const DELIMITER: char = ',';

/// Split a stored comma-joined column into its elements: empties dropped, each element trimmed.
/// One splitter for `tags`, `preload` and `owner`, so a stray space around a delimiter is never
/// read as part of an element.
pub fn split(raw: &str) -> Vec<String> {
    raw.split(DELIMITER)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_trims_and_drops_empties() {
        assert_eq!(split("a,b"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            split(" a , b "),
            vec!["a".to_string(), "b".to_string()],
            "a space around the delimiter is never part of an element"
        );
        assert_eq!(
            split("a,,b"),
            vec!["a".to_string(), "b".to_string()],
            "a doubled delimiter is dropped, not a blank element"
        );
        assert!(split("").is_empty());
        assert!(split("  ").is_empty());
    }
}
