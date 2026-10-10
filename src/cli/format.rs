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
use crate::store::Summary;
use crate::store::links::Dangling;
use crate::store::stats::Stats;

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

/// Render hits for a terminal, ranked first. A hit carries no body (SPEC §10): key, title,
/// tags and (semantic only) the distance.
fn render_hits(hits: &[Hit]) -> Result<String> {
    if hits.is_empty() {
        return Ok("(no matches)".to_string());
    }
    let mut out = String::new();
    for (rank, hit) in hits.iter().enumerate() {
        let distance = match hit.distance {
            Some(d) => format!("{d:.4}"),
            None => "-".to_string(),
        };
        out.push_str(&format!(
            "{}. {:>8}  {}\n   {}\n",
            rank + 1,
            distance,
            hit.key,
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
    let mut out = format!("{}\n  title   {}\n", mem.key, mem.title);
    if !mem.tags.is_empty() {
        out.push_str(&format!("  tags    {}\n", mem.tags.join(", ")));
    }
    if let Some(source) = &mem.source {
        out.push_str(&format!("  source  {source}\n"));
    }
    out.push_str(&format!("  times   {} -> {}\n", mem.created, mem.updated));
    out.push_str(&format!(
        "  use     {} visit(s), {} useful\n",
        mem.visits, mem.times_useful
    ));
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

/// One memory-index row, as `tabled` renders it.
#[derive(Tabled)]
struct ListRow {
    key: String,
    tags: String,
    title: String,
}

/// Render the memory index (`list`): a bordered table, or JSON for scripts. Bodies are left
/// out on purpose (SPEC §14) — key, tags and title only.
pub fn render_summaries(rows: &[Summary], json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(rows)?);
    }
    if rows.is_empty() {
        return Ok("(no memories)".to_string());
    }
    let table_rows: Vec<ListRow> = rows
        .iter()
        .map(|s| ListRow {
            key: s.key.clone(),
            tags: s.tags.join(", "),
            title: s.title.clone(),
        })
        .collect();
    let mut table = Table::new(table_rows);
    table.with(Style::psql());
    Ok(format!("{table}\n"))
}

/// Render the store digest (`stats`): a block per section, or JSON for scripts.
///
/// Numbers are `.0`-rounded for a quick scan; `--json` keeps the exact counts.
pub fn render_stats(s: &Stats, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(s)?);
    }
    let mut out = String::new();

    out.push_str("store\n");
    out.push_str(&format!("  memories       {}\n", s.memories));
    out.push_str(&format!("  on disk        {}\n", human_bytes(s.db_bytes)));
    out.push_str(&format!(
        "  bodies         avg {:.0} / min {} / max {} chars ({} total)\n",
        s.avg_body_chars(),
        s.body_min,
        s.body_max,
        s.body_chars
    ));
    out.push_str(&format!(
        "  titles         avg {:.0} chars\n",
        avg(s.title_chars, s.memories)
    ));
    out.push_str(&format!(
        "  sources        {}/{} ({:.0}%)\n",
        s.with_source,
        s.memories,
        pct(s.with_source, s.memories)
    ));
    out.push_str(&format!(
        "  activity       {} in last 7d, {} in last 30d\n",
        s.added_7d, s.added_30d
    ));
    match (&s.oldest, &s.newest) {
        (Some(a), Some(b)) => out.push_str(&format!("  span           {a} -> {b}\n")),
        _ => out.push_str("  span           (empty)\n"),
    }

    out.push_str("\ntags\n");
    out.push_str(&format!("  distinct       {}\n", s.distinct_tags));
    out.push_str(&format!(
        "  placements     {} ({:.1} per memory)\n",
        s.tag_assignments,
        s.avg_tags_per_memory()
    ));
    out.push_str(&format!(
        "  spread         {:.1} memories per tag\n",
        s.avg_memories_per_tag()
    ));
    out.push_str(&format!(
        "  coverage       {}/{} memories tagged ({:.0}%)\n",
        s.tagged,
        s.memories,
        pct(s.tagged, s.memories)
    ));
    out.push_str(&format!(
        "  bare           {} memories with no tag\n",
        s.untagged
    ));
    out.push_str(&format!("  singletons     {}\n", s.singleton_tags));
    if s.top_tags.is_empty() {
        out.push_str("  top            (none)\n");
    } else {
        let top = s
            .top_tags
            .iter()
            .map(|(tag, n)| format!("{tag} {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("  top            {top}\n"));
    }

    out.push_str("\nlinks\n");
    out.push_str(&format!("  edges          {}\n", s.edges));
    out.push_str(&format!(
        "  linked         {}/{} ({:.0}%)\n",
        s.linked,
        s.memories,
        pct(s.linked, s.memories)
    ));
    out.push_str(&format!("  orphans        {}\n", s.orphans));
    out.push_str(&format!("  dangling       {}\n", s.dangling));
    out.push_str(&format!(
        "  clusters       {} component(s), largest {}\n",
        s.link_components, s.largest_component
    ));

    out.push_str("\nuse\n");
    out.push_str(&format!("  visits         {}\n", s.visits_total));
    out.push_str(&format!("  useful         {}\n", s.useful_total));
    if s.top_useful.is_empty() {
        out.push_str("  top useful     (none)\n");
    } else {
        let top = s
            .top_useful
            .iter()
            .map(|(key, n)| format!("{key} {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("  top useful     {top}\n"));
    }

    Ok(out)
}

/// `total / count`, guarding the empty store.
fn avg(total: i64, count: i64) -> f64 {
    if count <= 0 {
        0.0
    } else {
        total as f64 / count as f64
    }
}

/// `part / whole` as a percentage, guarding the empty store.
fn pct(part: i64, whole: i64) -> f64 {
    avg(part, whole) * 100.0
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
            title: "GitOps & registry".into(),
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
            title: "Note".into(),
            body: "first line\nsecond line".into(),
            source: Some("tests".into()),
            doc_hash: "abc".into(),
            tags: vec!["k8s".into()],
            created: "2026-01-01 00:00:00".into(),
            updated: "2026-01-02 00:00:00".into(),
            visits: 3,
            times_useful: 1,
        }
    }

    #[test]
    fn memory_renders_a_block_and_an_object() {
        let block = render_memory(&memory(), false).unwrap();
        assert!(block.contains("infra-note"), "{block}");
        assert!(block.contains("  title   Note"), "{block}");
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
    fn summaries_render_a_table_and_json_without_bodies() {
        let rows = vec![Summary {
            key: "infra-note".into(),
            title: "Note".into(),
            tags: vec!["k8s".into()],
        }];
        let table = render_summaries(&rows, false).unwrap();
        assert!(
            table.contains("infra-note") && table.contains("tags"),
            "{table}"
        );

        let json = render_summaries(&rows, true).unwrap();
        assert!(json.contains("\"key\": \"infra-note\""), "{json}");
        assert!(json.contains("\"k8s\""), "{json}");
        assert!(
            !json.contains("\"body\""),
            "list must not carry bodies: {json}"
        );

        assert_eq!(render_summaries(&[], false).unwrap(), "(no memories)");
    }

    #[test]
    fn human_bytes_scales_and_never_abbreviates_small_values() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        // Beyond the last unit, the value keeps growing in GiB.
        assert_eq!(human_bytes(2048 * 1024 * 1024 * 1024), "2048.0 GiB");
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
