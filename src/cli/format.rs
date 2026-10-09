//! Output formatting: bordered tables for humans, `--json` for scripts (PLAN §6 P6).
//!
//! `--json` is the stable, machine-readable face of every command; the human path is a
//! convenience. Memory bodies are too dense for a table, so they render as blocks; the
//! tag list is a `tabled` table.

use tabled::settings::Style;
use tabled::{Table, Tabled};

use crate::error::Result;
use crate::search::{Bundle, Hit};
use crate::store::Memory;
use crate::store::links::Dangling;

/// Render a search bundle for a terminal (or `--json`), ranked first.
pub fn render_bundle(bundle: &Bundle, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(bundle)?);
    }
    let mut out = render_hits(&bundle.hits)?;
    if !bundle.neighbors.is_empty() {
        out.push_str(&format!("\n{} linked:\n", bundle.neighbors.len()));
        for n in &bundle.neighbors {
            out.push_str(&format!(
                "  hop {}  [[{}]] -> {}  {}\n",
                n.hop, n.via, n.memory.key, n.memory.title
            ));
        }
    }
    Ok(out)
}

/// Render hits for a terminal, ranked first.
fn render_hits(hits: &[Hit]) -> Result<String> {
    if hits.is_empty() {
        return Ok("(no matches)".to_string());
    }
    let mut out = String::new();
    for (rank, hit) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {:.4}  {}  [{}]\n   {}\n",
            rank + 1,
            hit.score,
            hit.key,
            hit.scope,
            hit.title
        ));
        if !hit.tags.is_empty() {
            out.push_str(&format!("   tags: {}\n", hit.tags.join(", ")));
        }
    }
    Ok(out)
}

/// Render one memory (`show`): a readable block, or JSON for scripts.
pub fn render_memory(mem: &Memory, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(mem)?);
    }
    let mut out = format!(
        "{}  [{}/{}]\n  title   {}\n",
        mem.key,
        mem.scope,
        mem.type_.as_str(),
        mem.title
    );
    if !mem.tags.is_empty() {
        out.push_str(&format!("  tags    {}\n", mem.tags.join(", ")));
    }
    if let Some(source) = &mem.source {
        out.push_str(&format!("  source  {source}\n"));
    }
    out.push_str(&format!("  times   {} -> {}\n", mem.created, mem.updated));
    out.push_str("  body\n");
    for line in mem.body.lines() {
        out.push_str(&format!("    {line}\n"));
    }
    Ok(out)
}

/// A tag with its reference count, as `tabled` renders it.
#[derive(Tabled)]
struct TagRow {
    tag: String,
    count: i64,
}

/// Render the tag vocabulary (`tags`): a bordered table, or JSON for scripts.
pub fn render_tags(tags: &[(String, i64)], json: bool) -> Result<String> {
    if json {
        let rows: Vec<_> = tags
            .iter()
            .map(|(tag, count)| serde_json::json!({ "tag": tag, "count": count }))
            .collect();
        return Ok(serde_json::to_string_pretty(&rows)?);
    }
    if tags.is_empty() {
        return Ok("(no tags)".to_string());
    }
    let rows: Vec<TagRow> = tags
        .iter()
        .map(|(tag, count)| TagRow {
            tag: tag.clone(),
            count: *count,
        })
        .collect();
    let mut table = Table::new(rows);
    table.with(Style::psql());
    Ok(format!("{table}\n"))
}

/// Render the dangling-link audit (`links`): one line per unresolved `[[key]]`, or JSON.
pub fn render_dangling(dangling: &[Dangling], json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(dangling)?);
    }
    if dangling.is_empty() {
        return Ok("(no dangling links)".to_string());
    }
    let mut out = String::new();
    for d in dangling {
        out.push_str(&format!(
            "[[{}]]  <-  {}\n",
            d.to_key,
            d.from_keys.join(", ")
        ));
    }
    Ok(out)
}

/// Bytes -> a short human string (`1.2 GiB`).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit() -> Hit {
        Hit {
            id: 1,
            key: "infra-gitops".into(),
            scope: "infra".into(),
            type_: "fact".into(),
            title: "GitOps & registry".into(),
            body: "argocd".into(),
            score: 0.5,
            distance: Some(0.25),
            tags: vec!["k8s".into()],
        }
    }

    #[test]
    fn bundle_json_roundtrips() {
        let bundle = Bundle {
            hits: vec![hit()],
            ..Default::default()
        };
        let json = render_bundle(&bundle, true).unwrap();
        assert!(json.contains("\"infra-gitops\""), "{json}");
        assert!(json.trim_start().starts_with('{'), "{json}");
    }

    #[test]
    fn empty_is_explicit() {
        assert_eq!(render_hits(&[]).unwrap(), "(no matches)");
        let json = render_bundle(&Bundle::default(), true).unwrap();
        assert!(json.contains("\"hits\": []"), "{json}");
    }

    fn memory() -> Memory {
        Memory {
            id: Some(7),
            key: "infra-note".into(),
            scope: "infra".into(),
            type_: crate::store::MemoryType::Signpost,
            title: "Note".into(),
            body: "first line\nsecond line".into(),
            source: Some("tests".into()),
            hash: "abc".into(),
            tags: vec!["k8s".into()],
            created: "2026-01-01 00:00:00".into(),
            updated: "2026-01-02 00:00:00".into(),
        }
    }

    #[test]
    fn memory_renders_a_block_and_an_object() {
        let block = render_memory(&memory(), false).unwrap();
        assert!(block.contains("infra-note  [infra/signpost]"), "{block}");
        assert!(block.contains("    second line"), "{block}");

        let json = render_memory(&memory(), true).unwrap();
        assert!(json.trim_start().starts_with('{'), "{json}");
        assert!(json.contains("\"infra-note\""), "{json}");
    }

    #[test]
    fn tags_render_a_table_and_json() {
        let tags = vec![("infra".to_string(), 1i64), ("k8s".to_string(), 2)];
        let table = render_tags(&tags, false).unwrap();
        assert!(table.contains("k8s") && table.contains("count"), "{table}");

        let json = render_tags(&tags, true).unwrap();
        assert!(json.contains("\"tag\": \"k8s\""), "{json}");
        assert!(json.contains("\"count\": 2"), "{json}");

        assert_eq!(render_tags(&[], false).unwrap(), "(no tags)");
    }

    #[test]
    fn dangling_renders_and_serialises() {
        assert_eq!(render_dangling(&[], false).unwrap(), "(no dangling links)");
        let dangling = vec![Dangling {
            to_key: "ghost".into(),
            from_keys: vec!["a".into(), "b".into()],
        }];
        let text = render_dangling(&dangling, false).unwrap();
        assert!(text.contains("[[ghost]]"), "{text}");
        assert!(text.contains("a, b"), "{text}");
        let json = render_dangling(&dangling, true).unwrap();
        assert!(json.contains("\"ghost\""), "{json}");
    }
}
