//! Store statistics for the `memory_stats` tool: size, spread, tag and link
//! shape, and recent activity — read-only aggregates over `memory` and its derived indexes.
//!
//! Everything here is cheap: counts and `length()` aggregates run in SQLite, and the tag shape
//! reuses the same column scan as `tags::list`. No bodies are ever loaded.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;
use serde::Serialize;

use crate::error::Result;
use crate::store::{encode, links, tags, version};

/// How many of the busiest tags the digest carries.
pub const TOP_TAGS: usize = 10;

/// How many of the most-useful memories the digest carries.
pub const TOP_USEFUL: usize = 5;

/// How many read-but-never-upvoted memories the digest carries.
pub const TOP_UNVOTED: usize = 5;

/// Minimum `visits` for a memory to count as "hot but unvoted" (a single read is a glance, not a hit).
pub const UNVOTED_MIN_VISITS: i64 = 2;

/// A read-only digest of the store (the `memory_stats` tool).
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    /// Rows in `memory`.
    pub memories: i64,
    /// Bytes on disk (`.db` + `-wal` + `-shm`), read from the open connection's store file; `0`
    /// for an in-memory store, which has no file to size.
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
    /// Rows with NO owner because somebody released them: `owner IS NULL` but `asserted_by` names a
    /// pen. This is the commons, and anyone may write or adopt.
    pub commons: i64,
    /// Rows with no owner AND no recorded author — nobody is answerable and nobody is named. Almost
    /// always a WRITE BUG rather than a fact about the row (the HTTP door did not stamp
    /// `asserted_by` until 2026-10-23, so every row it wrote landed here). They need an owner to
    /// CLAIM them, not to be silently granted to everyone (Stet, review of #122).
    pub unattributed: i64,
    /// Total tag placements (the sum of the per-tag counts).
    pub tag_assignments: i64,
    /// Distinct tags.
    pub distinct_tags: i64,
    /// Memories with at least one tag.
    pub tagged: i64,
    /// Memories with no tags at all (bare).
    pub untagged: i64,
    /// Tags used by exactly one memory.
    pub singleton_tags: i64,
    /// The busiest tags, count-desc then name — at most [`TOP_TAGS`] of them.
    pub top_tags: Vec<(String, i64)>,
    /// The full tag vocabulary, count-desc then name — empty unless asked for (see
    /// [`vocabulary`]). Tags are a primary filter, so the *whole* set is what stops a caller
    /// inventing a synonym; `top_tags` is only its head, and the sprawl lives in its tail.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all_tags: Vec<(String, i64)>,
    /// `memory_link` edges.
    pub edges: i64,
    /// Memories touching at least one link (incoming or outgoing).
    pub linked: i64,
    /// Memories with no links at all.
    pub orphans: i64,
    /// `[[key]]` targets with no matching row.
    pub dangling: i64,
    /// Connected components of the link graph with two or more memories (the dense clusters).
    pub link_components: i64,
    /// Memories in the largest link component (`0` for an empty store).
    pub largest_component: i64,
    /// Sum of `visits` across the store.
    pub visits_total: i64,
    /// Sum of `times_useful` across the store.
    pub useful_total: i64,
    /// The most-useful memories (key, `times_useful`), count-desc then key — at most [`TOP_USEFUL`].
    pub top_useful: Vec<(String, i64)>,
    /// Candidates the store's own ranking may be starving: memories read at least
    /// [`UNVOTED_MIN_VISITS`] times but never marked useful (key, `visits`), visit-desc then key —
    /// at most [`TOP_UNVOTED`]. Surfaced so a hot-but-ignored memory can be judged, not silently
    /// forgotten.
    pub unvoted_hot: Vec<(String, i64)>,
    /// Oldest `created` timestamp, or `None` for an empty store.
    pub oldest: Option<String>,
    /// Newest `created` timestamp, or `None` for an empty store.
    pub newest: Option<String>,
    /// Memories created in the last 7 days.
    pub added_7d: i64,
    /// Memories created in the last 30 days.
    pub added_30d: i64,
    /// Documents still owing the vector index — the encoder's backlog. A non-empty backlog is
    /// NORMAL (a write schedules its document and returns; see `ENCODE-WORKER.md`), so this is a
    /// metric and never a readiness gate.
    pub encode_pending: i64,
    /// Entries the encoder has given up on after `encode::MAX_ATTEMPTS`. Non-zero means a document
    /// that cannot be embedded and is therefore silently NOT searchable — the one queue number that
    /// is a defect rather than a load.
    pub encode_failed: i64,
    /// Age of the oldest pending document, in seconds — `None` when nothing is queued. If this only
    /// grows, the worker is not running or not keeping up; nothing else in the store would show it.
    pub encode_oldest_s: Option<i64>,
    /// Rows in `memory_version` — the states writes have superseded (migration 0011). Zero is normal
    /// on a young store; it only grows, because nothing but `memory_history_erase` removes a row.
    pub versions: i64,
    /// Bytes of snapshot content those versions hold, so what history costs is VISIBLE rather than a
    /// surprise at the next backup.
    pub version_bytes: i64,
    /// Keys whose `memory` row is GONE while their history remains. For an orphan the newest version
    /// IS the content it held, so these are the rows a retention pass must not thin.
    pub version_orphans: i64,
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

/// The full tag vocabulary with reference counts, count-desc then name. The store keeps no tag
/// table (SPEC §5/§7 — nothing to keep in sync), so this is derived on demand from the one column
/// that holds them; it is the *readable* half of tag hygiene, and a rename/merge is the write half.
///
/// [`compute`] leaves [`Stats::all_tags`] empty (the digest stays lean); a caller that wants the
/// whole vocabulary fills it from here.
pub fn vocabulary(conn: &Connection) -> Result<Vec<(String, i64)>> {
    vocabulary_seen(conn, &crate::store::audience::Reader::everything())
}

/// The whole vocabulary as one reader sees it.
pub fn vocabulary_seen(
    conn: &Connection,
    reader: &crate::store::audience::Reader,
) -> Result<Vec<(String, i64)>> {
    let mut counts = tags::list_seen(conn, reader)?; // (tag, count), name-ascending
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(counts)
}

/// Compute the digest. `db_bytes` is read from the connection's store file ([`disk_bytes`]), so it
/// is correct wherever the digest is taken; it is `0` only for an in-memory store, which has no
/// file to size. (Leaving it `0` here and expecting the *caller* to fill it made the field lie:
/// every MCP consumer saw a size of zero, which reads as an empty database.)
pub fn compute(conn: &Connection) -> Result<Stats> {
    compute_seen(conn, &crate::store::audience::Reader::everything())
}

/// The digest as ONE READER sees it.
///
/// ★ EVERY NUMBER HERE IS A DISCLOSURE SURFACE. A memory count that includes a row the reader cannot
/// open, a tag only private rows carry, an edge count that moves when a private link is added — each
/// is a one-bit leak, and two samples take the difference. So the predicate is applied to every
/// aggregate, and the graph/version halves are computed over the visible subset (or left out, when
/// the subset cannot be described without naming what is hidden).
pub fn compute_seen(conn: &Connection, reader: &crate::store::audience::Reader) -> Result<Stats> {
    let (pred, pred_binds) = reader.sql();
    let one = |sql: &str| -> Result<i64> {
        Ok(
            conn.query_row(sql, rusqlite::params_from_iter(pred_binds.iter()), |r| {
                r.get(0)
            })?,
        )
    };
    // ★ "NOBODY OWNS IT" AND "NOBODY IS NAMED" ARE DIFFERENT FACTS that share one value (`owner`
    // NULL). A disclaimed row keeps its `asserted_by`, so the two are separable without a new column
    // — and a report that merged them would call a bug a commons.
    let (commons, unattributed): (i64, i64) = conn.query_row(
        "SELECT sum(CASE WHEN lower(trim(asserted_by)) NOT IN ('', 'anonymous') THEN 1 ELSE 0 END),
                sum(CASE WHEN lower(trim(asserted_by)) IN ('', 'anonymous') THEN 1 ELSE 0 END)
           FROM memory WHERE owner IS NULL",
        [],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            ))
        },
    )?;
    let db_bytes = conn.path().map(|p| disk_bytes(Path::new(p))).unwrap_or(0);
    let memories: i64 = conn.query_row(
        &format!("SELECT count(*) FROM memory WHERE {pred}"),
        rusqlite::params_from_iter(pred_binds.iter()),
        |r| r.get(0),
    )?;

    // Body/title sums run in SQLite via `length()` (characters, not bytes) — bodies stay on disk.
    let (body_chars, body_min, body_max, title_chars): (i64, i64, i64, i64) = conn.query_row(
        &format!(
            "SELECT coalesce(sum(length(body)), 0), coalesce(min(length(body)), 0), \
                    coalesce(max(length(body)), 0), coalesce(sum(length(title)), 0) \
               FROM memory WHERE {pred}"
        ),
        rusqlite::params_from_iter(pred_binds.iter()),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let with_source = one(&format!(
        "SELECT count(*) FROM memory WHERE source IS NOT NULL AND trim(source) <> '' AND ({pred})"
    ))?;
    let tagged = one(&format!(
        "SELECT count(*) FROM memory WHERE tags <> '' AND ({pred})"
    ))?;

    let mut counts = tags::list_seen(conn, reader)?; // (tag, count), name-ascending
    let distinct_tags = counts.len() as i64;
    let tag_assignments: i64 = counts.iter().map(|(_, c)| c).sum();
    let singleton_tags = counts.iter().filter(|(_, c)| *c == 1).count() as i64;
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.truncate(TOP_TAGS);

    // Edge counts are MEMORY edges: a `[[prj:x]]` link names the other table, so it is not part of
    // the memory graph's degree, orphans or components — with bare keys, a project edge would
    // otherwise look like an edge to a memory that happens to share its name.
    let edges: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM memory_link l JOIN memory m ON m.id = l.from_id \
              WHERE l.to_type = 'memory' AND ({pred})"
        ),
        rusqlite::params_from_iter(pred_binds.iter()),
        |r| r.get(0),
    )?;
    let visible_keys: Option<std::collections::HashSet<String>> = match reader.visible_ids(conn)? {
        None => None,
        Some(ids) => {
            let mut set = std::collections::HashSet::new();
            let mut stmt = conn.prepare("SELECT key FROM memory WHERE id = ?1")?;
            for id in ids {
                if let Ok(key) = stmt.query_row([id], |r| r.get::<_, String>(0)) {
                    set.insert(key);
                }
            }
            Some(set)
        }
    };
    let dangling = links::dangling(conn)?
        .into_iter()
        .filter(|d| match &visible_keys {
            None => true,
            // A dangling edge named only by rows the reader cannot see is not their rot to see.
            Some(keys) => d.from_keys.iter().any(|k| keys.contains(k)),
        })
        .count() as i64;
    // A memory is linked if it appears as an edge source or resolves as an edge target.
    let orphans: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM memory m \
              WHERE m.id NOT IN (SELECT from_id FROM memory_link) \
                AND m.key NOT IN (SELECT to_key FROM memory_link WHERE to_type = 'memory') \
                AND ({pred})"
        ),
        rusqlite::params_from_iter(pred_binds.iter()),
        |r| r.get(0),
    )?;

    let oldest: Option<String> = conn.query_row(
        &format!("SELECT min(created) FROM memory WHERE {pred}"),
        rusqlite::params_from_iter(pred_binds.iter()),
        |r| r.get(0),
    )?;
    let newest: Option<String> = conn.query_row(
        &format!("SELECT max(created) FROM memory WHERE {pred}"),
        rusqlite::params_from_iter(pred_binds.iter()),
        |r| r.get(0),
    )?;
    let added_7d = one(&format!(
        "SELECT count(*) FROM memory WHERE created >= datetime('now', '-7 days') AND ({pred})"
    ))?;
    let added_30d = one(&format!(
        "SELECT count(*) FROM memory WHERE created >= datetime('now', '-30 days') AND ({pred})"
    ))?;

    // Usage telemetry: the two counters, plus the most-useful memories (the store's own gold).
    let (visits_total, useful_total): (i64, i64) = conn.query_row(
        &format!(
            "SELECT coalesce(sum(visits), 0), coalesce(sum(times_useful), 0) FROM memory WHERE {pred}"
        ),
        rusqlite::params_from_iter(pred_binds.iter()),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let top_useful: Vec<(String, i64)> = {
        // ★ BARE `?` THROUGHOUT, bound in TEXT ORDER. Mixing `?1` with a `?` that appears earlier
        // makes SQLite number the bare one first and then report a parameter count no caller can
        // satisfy — the store paid an hour for that once already.
        let mut stmt = conn.prepare(&format!(
            "SELECT key, times_useful FROM memory WHERE times_useful > 0 AND ({pred}) \
             ORDER BY times_useful DESC, key LIMIT ?"
        ))?;
        let mut binds: Vec<rusqlite::types::Value> =
            pred_binds.iter().cloned().map(Into::into).collect();
        binds.push((TOP_USEFUL as i64).into());
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // Hot but unvoted: read repeatedly, never marked useful — the store's own blind spot.
    let unvoted_hot: Vec<(String, i64)> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT key, visits FROM memory WHERE times_useful = 0 AND visits >= ? AND ({pred}) \
             ORDER BY visits DESC, key LIMIT ?"
        ))?;
        let mut binds: Vec<rusqlite::types::Value> = vec![UNVOTED_MIN_VISITS.into()];
        binds.extend(pred_binds.iter().cloned().map(Into::into));
        binds.push((TOP_UNVOTED as i64).into());
        let rows = stmt.query_map(rusqlite::params_from_iter(binds), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    // Dense clusters: connected components of the (undirected) link graph.
    let (link_components, largest_component) = match &visible_keys {
        None => component_sizes(conn)?,
        // Graph shape over a subset needs the subset; rather than name hidden rows by counting
        // components that include them, report the components among visible rows only.
        Some(keys) => component_sizes_of(conn, keys)?,
    };

    // The encode queue's shape: how much work, how much of it is broken, how long the oldest wait
    // has been. Derived state, but the only place the store reports on its own indexing.
    let queue = encode::counts(conn)?;

    // The version side table, reported so history cannot grow unseen: rows, bytes, and the ORPHANS
    // (history whose memory is gone, and whose newest version is therefore the only copy of it).
    let (versions, version_bytes) = version::count(conn)?;
    // A version holds the BODY, so history is counted only for rows this reader may see. Orphan
    // history (no live row) is excluded rather than classed: its audience is the newest snapshot's,
    // and reporting it would name rows the reader never knew existed.
    let (versions, version_bytes) = if reader.visible_ids(conn)?.is_none() {
        (versions, version_bytes)
    } else {
        conn.query_row(
            &format!(
                "SELECT count(*), COALESCE(sum(length(v.content)), 0) FROM memory_version v \
                   JOIN memory m ON m.key = v.key AND v.type = 'memory' WHERE {pred}"
            ),
            rusqlite::params_from_iter(pred_binds.iter()),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
    };
    let version_orphans: i64 = conn.query_row(
        "SELECT count(DISTINCT v.key) FROM memory_version v \
          LEFT JOIN memory m ON m.key = v.key \
          WHERE m.id IS NULL",
        [],
        |r| r.get(0),
    )?;

    Ok(Stats {
        memories,
        db_bytes,
        body_chars,
        body_min,
        body_max,
        title_chars,
        with_source,
        tag_assignments,
        distinct_tags,
        tagged,
        untagged: memories - tagged,
        singleton_tags,
        top_tags: counts,
        all_tags: Vec::new(),
        edges,
        linked: memories - orphans,
        orphans,
        dangling,
        link_components,
        largest_component,
        visits_total,
        useful_total,
        top_useful,
        unvoted_hot,
        oldest,
        newest,
        added_7d,
        added_30d,
        encode_pending: queue.pending,
        encode_failed: queue.failed,
        encode_oldest_s: queue.oldest_s,
        versions,
        version_bytes,
        version_orphans,
        commons,
        unattributed,
    })
}

/// Link-graph shape as `(components, largest)`: the number of connected components holding two
/// or more memories, and the size of the largest component (`0` for an empty store). Isolated
/// memories are the `orphans` count, ignored here. Edges to a dangling `[[key]]` don't connect.
fn component_sizes(conn: &Connection) -> Result<(i64, i64)> {
    component_sizes_of(conn, &std::collections::HashSet::new())
}

/// Components over the nodes a reader may see. An empty `only` means every node.
fn component_sizes_of(
    conn: &Connection,
    only: &std::collections::HashSet<String>,
) -> Result<(i64, i64)> {
    let mut nodes: Vec<i64> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut stmt = conn.prepare("SELECT id, key FROM memory")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (id, key) = row?;
        if !only.is_empty() && !only.contains(&key) {
            continue;
        }
        by_key.insert(key, nodes.len());
        nodes.push(id);
    }
    let pos: HashMap<i64, usize> = nodes.iter().enumerate().map(|(i, id)| (*id, i)).collect();

    let mut parent: Vec<usize> = (0..nodes.len()).collect();
    let mut stmt =
        conn.prepare("SELECT from_id, to_key FROM memory_link WHERE to_type = 'memory'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (from_id, to_key) = row?;
        if let (Some(&a), Some(&b)) = (pos.get(&from_id), by_key.get(&to_key)) {
            union(&mut parent, a, b);
        }
    }

    let mut sizes: HashMap<usize, i64> = HashMap::new();
    for i in 0..parent.len() {
        let root = find(&mut parent, i);
        *sizes.entry(root).or_default() += 1;
    }
    let mut components = 0;
    let mut largest = 0;
    for &size in sizes.values() {
        if size >= 2 {
            components += 1;
        }
        largest = largest.max(size);
    }
    Ok((components, largest))
}

/// Union-find root with path halving.
fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// Merge the sets holding `a` and `b`.
fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[ra] = rb;
    }
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
            untagged: 0,
            singleton_tags: 0,
            top_tags: Vec::new(),
            all_tags: Vec::new(),
            edges: 0,
            linked: 0,
            orphans: 0,
            dangling: 0,
            link_components: 0,
            largest_component: 0,
            visits_total: 0,
            useful_total: 0,
            top_useful: Vec::new(),
            unvoted_hot: Vec::new(),
            oldest: None,
            newest: None,
            added_7d: 0,
            added_30d: 0,
            encode_pending: 0,
            encode_failed: 0,
            encode_oldest_s: None,
            versions: 0,
            version_bytes: 0,
            version_orphans: 0,
            commons: 0,
            unattributed: 0,
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
