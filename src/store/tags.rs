use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::error::Result;
use crate::store::codec;

pub fn normalize(raw: &str) -> Option<String> {
    let t = raw
        .trim()
        .to_lowercase()
        .replace(char::is_whitespace, "-")
        .replace(',', "-");
    let t = t.trim_matches('-');
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

pub fn encode(tags: &[String]) -> String {
    let mut tags: Vec<String> = tags.iter().filter_map(|t| normalize(t)).collect();
    tags.sort();
    tags.dedup();
    tags.join(",")
}

pub fn decode(raw: &str) -> Vec<String> {
    codec::split(raw)
}

pub fn keys_with(conn: &Connection, tag: &str) -> Result<Vec<(String, String)>> {
    keys_with_seen(conn, tag, &crate::store::audience::Reader::everything())
}

pub fn keys_with_seen(
    conn: &Connection,
    tag: &str,
    reader: &crate::store::audience::Reader,
) -> Result<Vec<(String, String)>> {
    let Some(want) = normalize(tag) else {
        return Ok(Vec::new());
    };
    let (predicate, binds) = reader.sql();
    let mut stmt = conn.prepare(&format!(
        "SELECT key, title, tags FROM memory WHERE {predicate} ORDER BY key"
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (key, title, tags) = row?;
        if decode(&tags).iter().any(|t| t == &want) {
            out.push((key, title));
        }
    }
    Ok(out)
}

pub fn list(conn: &Connection) -> Result<Vec<(String, i64)>> {
    list_seen(conn, &crate::store::audience::Reader::everything())
}

pub fn list_seen(
    conn: &Connection,
    reader: &crate::store::audience::Reader,
) -> Result<Vec<(String, i64)>> {
    let (predicate, binds) = reader.sql();
    let mut stmt = conn.prepare(&format!("SELECT tags FROM memory WHERE {predicate}"))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), |r| {
        r.get::<_, String>(0)
    })?;
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for row in rows {
        for tag in decode(&row?) {
            *counts.entry(tag).or_default() += 1;
        }
    }
    Ok(counts.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_case_and_space() {
        assert_eq!(normalize("  Rust Lang ").as_deref(), Some("rust-lang"));
        assert_eq!(normalize("a  b").as_deref(), Some("a--b"));
    }

    #[test]
    fn drops_empties() {
        assert_eq!(normalize("   "), None);
        assert_eq!(normalize("--"), None);
    }

    #[test]
    fn commas_cannot_break_the_stored_list() {
        assert_eq!(normalize("a,b").as_deref(), Some("a-b"));
    }

    #[test]
    fn encode_sorts_and_dedupes_then_decode_round_trips() {
        let raw = vec!["K8s".to_string(), "infra".to_string(), "k8s".to_string()];
        assert_eq!(encode(&raw), "infra,k8s");
        assert_eq!(
            decode("infra,k8s"),
            vec!["infra".to_string(), "k8s".to_string()]
        );
        assert!(decode("").is_empty());
    }
}
