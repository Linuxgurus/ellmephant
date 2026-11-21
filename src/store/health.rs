use rusqlite::Connection;
use serde::Serialize;

use crate::error::Result;
use crate::store::audience::Reader;
use crate::store::stats;

pub const SCOPE: &str = "visible";

pub const SCOPE_NOTE: &str = "No store total is reported: every count and share is computed through the caller's own Reader (shape (a), soma/ellmephant#218). Rows outside the caller's audience are absent, not counted; a total is withheld by decision, not by accident.";

pub const LOG_CLASS_PREFIXES: &[&str] = &[
    "alerts-",
    "hygiene-",
    "recess-",
    "maintenance-",
    "pr-watch-",
    "dispatch-",
    "offsite-",
    "cluster-watch-",
];

pub const BODY_CHARS_DEFINITION: &str = "body_chars is the sum of `length(body)` over VISIBLE memories — the rows this caller's Reader may see. No store-wide total is reported (shape (a)); a row outside the caller's audience is absent, not counted.";

pub const LOG_SHARE_DEFINITION: &str = "log_share is `log_chars / body_chars` over VISIBLE rows, as a percentage; `log_chars` sums the bodies of rows whose key begins, case-sensitively, with one of eight operational-log prefixes: alerts-, hygiene-, recess-, maintenance-, pr-watch-, dispatch-, offsite-, cluster-watch-.";

pub const DUP_PAIRS_DEFINITION: &str = "dup_pairs is what `memory_dupes` counts, not a number this call computes: VISIBLE pairs at or below the default cosine distance (0.05) whose `memory_dupes` class is `duplicate`; series, artifact and distinct pairs are excluded. Candidate pairs come from each row's 3 nearest neighbours (memory_dupes' NEIGHBOURS cap, k = 4 including self), not from an all-pairs search, so a mutual cluster of five or more near-identical rows can be undercounted. dup_pairs_min_distance is the smallest distance among those duplicate pairs, or null when there are none.";

pub const WOVEN_DEFINITION: &str = "woven is the percentage of VISIBLE rows with at least one `memory_link` edge in EITHER direction, where an edge counts only when both ends are visible; woven_inbound counts rows named as a memory target (`to_type = 'memory'`), woven_outbound counts rows that name a visible memory target (`to_type = 'memory'`). A row with both counts once in `woven`.";

pub const COLD30_DEFINITION: &str = "cold30 is the percentage of VISIBLE rows older than 30 days with `visits = 0` (never read); cold30_population is the number of visible rows older than 30 days. `visits` is the store's only read signal, so cold means never read, not 'not read in the last 30 days'.";

#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub scope: &'static str,
    pub scope_note: &'static str,

    pub rows: i64,
    pub body_chars: i64,
    pub log_chars: i64,
    pub log_share: Option<f64>,
    pub log_share_definition: &'static str,

    pub dup_pairs: Option<i64>,
    pub dup_pairs_min_distance: Option<f64>,
    pub dup_pairs_definition: &'static str,

    pub woven: Option<f64>,
    pub woven_inbound: Option<f64>,
    pub woven_outbound: Option<f64>,
    pub woven_definition: &'static str,

    pub cold30: Option<f64>,
    pub cold30_population: i64,
    pub cold30_definition: &'static str,

    pub tag_distinct: i64,
    pub tag_singletons: i64,
    pub rows_untagged: i64,

    pub edges: i64,
    pub edges_per_row: Option<f64>,
    pub dangling: i64,

    pub suppressed: Vec<String>,
}

fn percent(part: i64, whole: i64) -> Option<f64> {
    if whole <= 0 {
        None
    } else {
        Some(part as f64 * 100.0 / whole as f64)
    }
}

fn count(conn: &Connection, sql: &str, binds: &[String]) -> Result<i64> {
    Ok(conn.query_row(sql, rusqlite::params_from_iter(binds.iter()), |r| r.get(0))?)
}

pub fn compute_seen(conn: &Connection, reader: &Reader) -> Result<Health> {
    let s = stats::compute_seen(conn, reader)?;
    let (pred, pred_binds) = reader.sql();

    let log_clause = LOG_CLASS_PREFIXES
        .iter()
        .map(|_| "key GLOB ?")
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut log_binds = pred_binds.clone();
    for prefix in LOG_CLASS_PREFIXES {
        log_binds.push(format!("{prefix}*"));
    }
    let log_chars = count(
        conn,
        &format!(
            "SELECT coalesce(sum(length(body)), 0) FROM memory \
              WHERE ({pred}) AND ({log_clause})"
        ),
        &log_binds,
    )?;

    let mut inbound_binds = pred_binds.clone();
    inbound_binds.extend(pred_binds.iter().cloned());
    let woven_inbound = count(
        conn,
        &format!(
            "SELECT count(*) FROM memory m WHERE ({pred}) \
                AND m.key IN (SELECT l.to_key FROM memory_link l \
                                WHERE l.to_type = 'memory' \
                                  AND l.from_id IN (SELECT id FROM memory WHERE {pred}))"
        ),
        &inbound_binds,
    )?;
    let mut outbound_binds = pred_binds.clone();
    outbound_binds.extend(pred_binds.iter().cloned());
    let woven_outbound = count(
        conn,
        &format!(
            "SELECT count(*) FROM memory m WHERE ({pred}) \
                AND m.id IN (SELECT l.from_id FROM memory_link l \
                              WHERE l.to_type = 'memory' \
                                AND l.to_key IN (SELECT t.key FROM memory t WHERE {pred}))"
        ),
        &outbound_binds,
    )?;
    let mut woven_binds = pred_binds.clone();
    woven_binds.extend(pred_binds.iter().cloned());
    woven_binds.extend(pred_binds.iter().cloned());
    let woven = count(
        conn,
        &format!(
            "SELECT count(*) FROM memory m WHERE ({pred}) AND (\
                m.id IN (SELECT l.from_id FROM memory_link l \
                          WHERE l.to_type = 'memory' \
                            AND l.to_key IN (SELECT t.key FROM memory t WHERE {pred})) \
                OR m.key IN (SELECT l.to_key FROM memory_link l \
                              WHERE l.to_type = 'memory' \
                                AND l.from_id IN (SELECT id FROM memory WHERE {pred})))"
        ),
        &woven_binds,
    )?;

    let (cold30_population, cold30_count): (i64, i64) = conn.query_row(
        &format!(
            "SELECT count(*), coalesce(sum(CASE WHEN visits = 0 THEN 1 ELSE 0 END), 0) \
               FROM memory WHERE ({pred}) AND created < datetime('now', '-30 days')"
        ),
        rusqlite::params_from_iter(pred_binds.iter()),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let oldest_age_days: Option<f64> = match &s.oldest {
        Some(created) => Some(conn.query_row(
            "SELECT julianday('now') - julianday(?1)",
            rusqlite::params![created],
            |r| r.get(0),
        )?),
        None => None,
    };

    let dup_pairs: Option<i64> = None;
    let dup_pairs_min_distance: Option<f64> = None;

    let log_share = percent(log_chars, s.body_chars);
    let woven = percent(woven, s.memories);
    let woven_inbound = percent(woven_inbound, s.memories);
    let woven_outbound = percent(woven_outbound, s.memories);
    let cold30 = percent(cold30_count, cold30_population);
    let edges_per_row = if s.memories > 0 {
        Some(s.edges as f64 / s.memories as f64)
    } else {
        None
    };

    let mut suppressed = Vec::new();
    if cold30.is_none() {
        suppressed.push(match oldest_age_days {
            Some(age) => format!(
                "cold30: no visible rows older than 30 days — the 30d+ population is empty; the oldest visible row is {age:.1}d old"
            ),
            None => "cold30: no visible rows — the 30d+ population is empty".to_string(),
        });
    }
    if log_share.is_none() {
        suppressed.push(
            "log_share: no visible body characters — the share has no denominator".to_string(),
        );
    }
    if woven.is_none() {
        suppressed.push("woven: no visible rows — the share has no denominator".to_string());
    }
    if dup_pairs.is_none() {
        suppressed.push(
            "dup_pairs: not computed in this call — the pair count is the `memory_dupes` finder (class `duplicate` only, NEIGHBOURS = 3 cap), which returns the pairs and their distances; `dup_pairs_min_distance` is not computed here either. Call `memory_dupes` for that number, and do not read this null as 0."
                .to_string(),
        );
    }
    if edges_per_row.is_none() {
        suppressed
            .push("edges_per_row: no visible rows — the ratio has no denominator".to_string());
    }

    Ok(Health {
        scope: SCOPE,
        scope_note: SCOPE_NOTE,
        rows: s.memories,
        body_chars: s.body_chars,
        log_chars,
        log_share,
        log_share_definition: LOG_SHARE_DEFINITION,
        dup_pairs,
        dup_pairs_min_distance,
        dup_pairs_definition: DUP_PAIRS_DEFINITION,
        woven,
        woven_inbound,
        woven_outbound,
        woven_definition: WOVEN_DEFINITION,
        cold30,
        cold30_population,
        cold30_definition: COLD30_DEFINITION,
        tag_distinct: s.distinct_tags,
        tag_singletons: s.singleton_tags,
        rows_untagged: s.untagged,
        edges: s.edges,
        edges_per_row,
        dangling: s.dangling,
        suppressed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_ratio_is_none_not_zero() {
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(1, 0), None);
        assert_eq!(percent(0, 4), Some(0.0));
        assert_eq!(percent(1, 4), Some(25.0));
    }

    #[test]
    fn the_dup_pairs_definition_names_the_neighbour_cap() {
        assert!(DUP_PAIRS_DEFINITION.contains("3 nearest neighbours"));
        assert!(DUP_PAIRS_DEFINITION.contains("not from an all-pairs search"));
    }
}
