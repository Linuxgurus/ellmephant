//! Store statistics for `llmphant stats` (aliases `summary`/`stat`): size, spread, tag and link
//! shape, and recent activity — read-only aggregates over `memory` and its derived indexes.
//!
//! Everything here is cheap: counts and `length()` aggregates run in SQLite, and the tag shape
//! reuses the same column scan as `tags::list`. No bodies are ever loaded.

use std::path::Path;

use rusqlite::Connection;
use serde::Serialize;

use crate::error::Result;
use crate::store::{links, tags};

/// How many of the busiest tags the digest carries.
pub const TOP_TAGS: usize = 10;

/// A read-only digest of the store (`llmphant stats`).
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    /// Rows in `memory`.
    pub memories: i64,
    /// Bytes on disk (`.db` + `-wal` + `-shm`); set by the CLI, `0` in the store layer.
    pub db_bytes: u64,
    /// Total characters across bodies.
    pub body_chars: i64,
    /// Shortest body, in characters.
    pub body_min: i64,
    /// Longest body, in characters.
    pub body_max: i64,
    /// Total characters across titles.
    pub title_chars: i64,
    /// Memories carrying a `source`.
    pub with_source: i64,
    /// Total tag placements (the sum of the per-tag counts).
    pub tag_assignments: i64,
    /// Distinct tags.
    pub distinct_tags: i64,
    /// Memories with at least one tag.
    pub tagged: i64,
    /// Tags used by exactly one memory.
    pub singleton_tags: i64,
    /// The busiest tags, count-desc then name — at most [`TOP_TAGS`] of them.
    pub top_tags: Vec<(String, i64)>,
    /// `memory_link` edges.
    pub edges: i64,
    /// Memories touching at least one link (incoming or outgoing).
    pub linked: i64,
    /// Memories with no links at all.
    pub orphans: i64,
    /// `[[key]]` targets with no matching row.
    pub dangling: i64,
    /// Oldest `created` timestamp, or `None` for an empty store.
    pub oldest: Option<String>,
    /// Newest `created` timestamp, or `None` for an empty store.
    pub newest: Option<String>,
    /// Memories created in the last 7 days.
    pub added_7d: i64,
    /// Memories created in the last 30 days.
    pub added_30d: i64,
}

impl Stats {
    /// Mean body length in characters.
    pub fn avg_body_chars(&self) -> f64 {
        ratio(self.body_chars, self.memories)
    }

    /// Mean tags per memory.
    pub fn avg_tags_per_memory(&self) -> f64 {
        ratio(self.tag_assignments, self.memories)
    }

    /// Mean memories per distinct tag.
    pub fn avg_memories_per_tag(&self) -> f64 {
        ratio(self.tag_assignments, self.distinct_tags)
    }

    /// Share of memories that carry a `source`, `0.0..=1.0`.
    pub fn source_ratio(&self) -> f64 {
        ratio(self.with_source, self.memories)
    }

    /// Share of memories with at least one tag, `0.0..=1.0`.
    pub fn tagged_ratio(&self) -> f64 {
        ratio(self.tagged, self.memories)
    }
}

/// `a / b`, guarding the empty store (`0.0` rather than a NaN).
fn ratio(a: i64, b: i64) -> f64 {
    if b <= 0 { 0.0 } else { a as f64 / b as f64 }
}

/// Compute the digest. `db_bytes` is left `0`; the CLI fills it from [`disk_bytes`].
pub fn compute(conn: &Connection) -> Result<Stats> {
    let memories: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;

    // Body/title sums run in SQLite via `length()` (characters, not bytes) — bodies stay on disk.
    let (body_chars, body_min, body_max, title_chars): (i64, i64, i64, i64) = conn.query_row(
        "SELECT coalesce(sum(length(body)), 0), coalesce(min(length(body)), 0), \
                coalesce(max(length(body)), 0), coalesce(sum(length(title)), 0) FROM memory",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let with_source: i64 = conn.query_row(
        "SELECT count(*) FROM memory WHERE source IS NOT NULL AND trim(source) <> ''",
        [],
        |r| r.get(0),
    )?;
    let tagged: i64 = conn.query_row("SELECT count(*) FROM memory WHERE tags <> ''", [], |r| {
        r.get(0)
    })?;

    let mut counts = tags::list(conn)?; // (tag, count), name-ascending
    let distinct_tags = counts.len() as i64;
    let tag_assignments: i64 = counts.iter().map(|(_, c)| c).sum();
    let singleton_tags = counts.iter().filter(|(_, c)| *c == 1).count() as i64;
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.truncate(TOP_TAGS);

    let edges: i64 = conn.query_row("SELECT count(*) FROM memory_link", [], |r| r.get(0))?;
    let dangling = links::dangling(conn)?.len() as i64;
    // A memory is linked if it appears as an edge source or resolves as an edge target.
    let orphans: i64 = conn.query_row(
        "SELECT count(*) FROM memory m \
          WHERE m.id NOT IN (SELECT from_id FROM memory_link) \
            AND m.key NOT IN (SELECT to_key FROM memory_link)",
        [],
        |r| r.get(0),
    )?;

    let oldest: Option<String> =
        conn.query_row("SELECT min(created) FROM memory", [], |r| r.get(0))?;
    let newest: Option<String> =
        conn.query_row("SELECT max(created) FROM memory", [], |r| r.get(0))?;
    let added_7d: i64 = conn.query_row(
        "SELECT count(*) FROM memory WHERE created >= datetime('now', '-7 days')",
        [],
        |r| r.get(0),
    )?;
    let added_30d: i64 = conn.query_row(
        "SELECT count(*) FROM memory WHERE created >= datetime('now', '-30 days')",
        [],
        |r| r.get(0),
    )?;

    Ok(Stats {
        memories,
        db_bytes: 0,
        body_chars,
        body_min,
        body_max,
        title_chars,
        with_source,
        tag_assignments,
        distinct_tags,
        tagged,
        singleton_tags,
        top_tags: counts,
        edges,
        linked: memories - orphans,
        orphans,
        dangling,
        oldest,
        newest,
        added_7d,
        added_30d,
    })
}

/// On-disk size of the store: the `.db` file plus any `-wal`/`-shm` sidecars, in bytes.
pub fn disk_bytes(db: &Path) -> u64 {
    let mut total = std::fs::metadata(db).map(|m| m.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = db.as_os_str().to_os_string();
        sidecar.push(suffix);
        total += std::fs::metadata(&sidecar).map(|m| m.len()).unwrap_or(0);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(memories: i64, tag_assignments: i64, distinct_tags: i64) -> Stats {
        Stats {
            memories,
            db_bytes: 0,
            body_chars: 200,
            body_min: 10,
            body_max: 90,
            title_chars: 40,
            with_source: 1,
            tag_assignments,
            distinct_tags,
            tagged: 2,
            singleton_tags: 0,
            top_tags: Vec::new(),
            edges: 0,
            linked: 0,
            orphans: 0,
            dangling: 0,
            oldest: None,
            newest: None,
            added_7d: 0,
            added_30d: 0,
        }
    }

    #[test]
    fn averages_divide_and_guard_the_empty_store() {
        let s = sample(4, 8, 2);
        assert!((s.avg_body_chars() - 50.0).abs() < 1e-9);
        assert!((s.avg_tags_per_memory() - 2.0).abs() < 1e-9);
        assert!((s.avg_memories_per_tag() - 4.0).abs() < 1e-9);
        assert!((s.tagged_ratio() - 0.5).abs() < 1e-9);

        let empty = sample(0, 0, 0);
        assert_eq!(empty.avg_body_chars(), 0.0);
        assert_eq!(empty.avg_tags_per_memory(), 0.0);
        assert_eq!(empty.avg_memories_per_tag(), 0.0);
        assert_eq!(empty.source_ratio(), 0.0);
    }
}
