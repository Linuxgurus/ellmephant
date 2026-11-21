use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;
use serde::Serialize;

use crate::error::Result;
use crate::store::{encode, links, tags, version};

pub const TOP_TAGS: usize = 10;

pub const TOP_USEFUL: usize = 5;

pub const TOP_UNVOTED: usize = 5;

pub const UNVOTED_MIN_VISITS: i64 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub memories: i64,
    pub db_bytes: u64,
    pub body_chars: i64,
    pub body_min: i64,
    pub body_max: i64,
    pub title_chars: i64,
    pub with_source: i64,
    pub commons: i64,
    pub unattributed: i64,
    pub tag_assignments: i64,
    pub distinct_tags: i64,
    pub tagged: i64,
    pub untagged: i64,
    pub singleton_tags: i64,
    pub top_tags: Vec<(String, i64)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all_tags: Vec<(String, i64)>,
    pub edges: i64,
    pub linked: i64,
    pub orphans: i64,
    pub dangling: i64,
    pub link_components: i64,
    pub largest_component: i64,
    pub visits_total: i64,
    pub useful_total: i64,
    pub top_useful: Vec<(String, i64)>,
    pub unvoted_hot: Vec<(String, i64)>,
    pub oldest: Option<String>,
    pub newest: Option<String>,
    pub added_7d: i64,
    pub added_30d: i64,
    pub encode_pending: i64,
    pub encode_failed: i64,
    pub encode_oldest_s: Option<i64>,
    pub versions: i64,
    pub version_bytes: i64,
    pub version_orphans: i64,
}

impl Stats {
    pub fn avg_body_chars(&self) -> f64 {
        ratio(self.body_chars, self.memories)
    }

    pub fn avg_tags_per_memory(&self) -> f64 {
        ratio(self.tag_assignments, self.memories)
    }

    pub fn avg_memories_per_tag(&self) -> f64 {
        ratio(self.tag_assignments, self.distinct_tags)
    }

    pub fn source_ratio(&self) -> f64 {
        ratio(self.with_source, self.memories)
    }

    pub fn tagged_ratio(&self) -> f64 {
        ratio(self.tagged, self.memories)
    }
}

fn ratio(a: i64, b: i64) -> f64 {
    if b <= 0 { 0.0 } else { a as f64 / b as f64 }
}

pub fn vocabulary(conn: &Connection) -> Result<Vec<(String, i64)>> {
    vocabulary_seen(conn, &crate::store::audience::Reader::everything())
}

pub fn vocabulary_seen(
    conn: &Connection,
    reader: &crate::store::audience::Reader,
) -> Result<Vec<(String, i64)>> {
    let mut counts = tags::list_seen(conn, reader)?;
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(counts)
}

pub fn compute(conn: &Connection) -> Result<Stats> {
    compute_seen(conn, &crate::store::audience::Reader::everything())
}

pub fn compute_seen(conn: &Connection, reader: &crate::store::audience::Reader) -> Result<Stats> {
    let (pred, pred_binds) = reader.sql();
    let one = |sql: &str| -> Result<i64> {
        Ok(
            conn.query_row(sql, rusqlite::params_from_iter(pred_binds.iter()), |r| {
                r.get(0)
            })?,
        )
    };
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

    let mut counts = tags::list_seen(conn, reader)?;
    let distinct_tags = counts.len() as i64;
    let tag_assignments: i64 = counts.iter().map(|(_, c)| c).sum();
    let singleton_tags = counts.iter().filter(|(_, c)| *c == 1).count() as i64;
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.truncate(TOP_TAGS);

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
            Some(keys) => d.from_keys.iter().any(|k| keys.contains(k)),
        })
        .count() as i64;
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

    let (visits_total, useful_total): (i64, i64) = conn.query_row(
        &format!(
            "SELECT coalesce(sum(visits), 0), coalesce(sum(times_useful), 0) FROM memory WHERE {pred}"
        ),
        rusqlite::params_from_iter(pred_binds.iter()),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let top_useful: Vec<(String, i64)> = {
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

    let (link_components, largest_component) = match &visible_keys {
        None => component_sizes(conn)?,
        Some(keys) => component_sizes_of(conn, keys)?,
    };

    let queue = encode::counts(conn)?;

    let (versions, version_bytes) = version::count(conn)?;
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

fn component_sizes(conn: &Connection) -> Result<(i64, i64)> {
    component_sizes_of(conn, &std::collections::HashSet::new())
}

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

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[ra] = rb;
    }
}

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
