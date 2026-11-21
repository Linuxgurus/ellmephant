pub const DELIMITER: char = ',';

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
