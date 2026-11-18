//! The `memory_health` surface: one call, five numbers, and the definition that travels with each.
//!
//! **Shape (a) is decided** (soma/ellmephant#218, 2026-11-14): every number is computed through the
//! caller's own [`Reader`], the response says `scope: "visible"`, and NO store total is reported.
//! `Reader::everything()` is deliberately not used here. A row count is metadata, but making a health
//! metric depend on reading another pen's private rows is a read boundary moved for a nicer number;
//! reporting totals is shape (b), a separate reviewed change (#144 / `secret-tiering`).
//!
//! `suppressed` names a number that could not be computed. In particular `cold30` is `null`, never
//! `0`, when the 30d+ population is empty: `0` would say "nothing is cold", which is a different
//! claim from "nothing is measurable".
//!
//! Every number here is a cheap aggregate. The duplicate-pair count is deliberately NOT computed in
//! this call: `memory_dupes` is the finder that returns it, and running it inside the aggregate costs
//! one KNN query per visible row (measured 19.7 s on a 1922-row store, 99.5% of the call). So
//! `dup_pairs` is `null` with a `suppressed` line naming `memory_dupes`, never a silent `0` — the
//! same doctrine as `cold30`: an uncomputed number is not a zero.

use rusqlite::Connection;
use serde::Serialize;

use crate::error::Result;
use crate::store::audience::Reader;
use crate::store::stats;

/// The response's scope marker: these numbers describe the caller's visible rows, not the store.
pub const SCOPE: &str = "visible";

/// The one sentence that has to travel with a visible-scoped number, so it is not read as a whole-store
/// verdict (#104).
pub const SCOPE_NOTE: &str = "No store total is reported: every count and share is computed through the caller's own Reader (shape (a), soma/ellmephant#218). Rows outside the caller's audience are absent, not counted; a total is withheld by decision, not by accident.";

/// The operational-log key prefixes that make `log_chars` / `log_share`. This list is a design
/// decision (soma/ellmephant#218), so it lives in code with a comment rather than in a document.
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

/// `body_chars` definition, carried in the response so a later reader cannot recompute it differently.
pub const BODY_CHARS_DEFINITION: &str = "body_chars is the sum of `length(body)` over VISIBLE memories — the rows this caller's Reader may see. No store-wide total is reported (shape (a)); a row outside the caller's audience is absent, not counted.";

/// `log_share` definition. The eight prefixes are repeated here on purpose, and a test keeps this
/// string and [`LOG_CLASS_PREFIXES`] in step.
pub const LOG_SHARE_DEFINITION: &str = "log_share is `log_chars / body_chars` over VISIBLE rows, as a percentage; `log_chars` sums the bodies of rows whose key begins, case-sensitively, with one of eight operational-log prefixes: alerts-, hygiene-, recess-, maintenance-, pr-watch-, dispatch-, offsite-, cluster-watch-.";

/// `dup_pairs` definition. It describes what `memory_dupes` counts, because that finder — not this
/// aggregate — returns the number. The neighbour cap is named because it bounds what the number can
/// mean: this is not an all-pairs count.
pub const DUP_PAIRS_DEFINITION: &str = "dup_pairs is what `memory_dupes` counts, not a number this call computes: VISIBLE pairs at or below the default cosine distance (0.05) whose `memory_dupes` class is `duplicate`; series, artifact and distinct pairs are excluded. Candidate pairs come from each row's 3 nearest neighbours (memory_dupes' NEIGHBOURS cap, k = 4 including self), not from an all-pairs search, so a mutual cluster of five or more near-identical rows can be undercounted. dup_pairs_min_distance is the smallest distance among those duplicate pairs, or null when there are none.";

/// `woven` definition. Three numbers, one definition, and the direction named.
pub const WOVEN_DEFINITION: &str = "woven is the percentage of VISIBLE rows with at least one `memory_link` edge in EITHER direction, where an edge counts only when both ends are visible; woven_inbound counts rows named as a memory target (`to_type = 'memory'`), woven_outbound counts rows that name a visible memory target (`to_type = 'memory'`). A row with both counts once in `woven`.";

/// `cold30` definition. The store has `visits` (a lifetime count), not a last-visit timestamp, so
/// "cold" can only mean never read.
pub const COLD30_DEFINITION: &str = "cold30 is the percentage of VISIBLE rows older than 30 days with `visits = 0` (never read); cold30_population is the number of visible rows older than 30 days. `visits` is the store's only read signal, so cold means never read, not 'not read in the last 30 days'.";

/// One health reading over the rows a caller may see.
#[derive(Debug, Clone, Serialize)]
pub struct Health {
    /// `visible` — the response's own statement of what it counted (shape (a)).
    pub scope: &'static str,
    /// Why there is no store total, in one sentence.
    pub scope_note: &'static str,

    /// Visible memories.
    pub rows: i64,
    /// Visible body characters.
    pub body_chars: i64,
    /// Visible body characters in operational-log rows.
    pub log_chars: i64,
    /// `log_chars / body_chars` as a percentage; `None` when there are no visible characters.
    pub log_share: Option<f64>,
    /// The predicate `log_share` used, always in the response.
    pub log_share_definition: &'static str,

    /// Visible pairs whose class is `duplicate`, as `memory_dupes` counts them; `None` here because
    /// this aggregate does not run that finder (the `suppressed` line names it).
    pub dup_pairs: Option<i64>,
    /// Smallest distance among those pairs, in the same 4dp form as `memory_dupes`; `None` here for
    /// the same reason, and `None` from `memory_dupes` when there are no duplicate pairs.
    pub dup_pairs_min_distance: Option<f64>,
    /// What `memory_dupes` counts for `dup_pairs`.
    pub dup_pairs_definition: &'static str,

    /// Percentage of visible rows with a link edge in either direction.
    pub woven: Option<f64>,
    /// Percentage of visible rows named as a memory target.
    pub woven_inbound: Option<f64>,
    /// Percentage of visible rows that name a target.
    pub woven_outbound: Option<f64>,
    /// What `woven` and its two directions counted.
    pub woven_definition: &'static str,

    /// Percentage of visible 30d+ rows that have never been read; `None` when the population is empty.
    pub cold30: Option<f64>,
    /// Visible rows older than 30 days — the denominator.
    pub cold30_population: i64,
    /// What `cold30` counted.
    pub cold30_definition: &'static str,

    /// Distinct tags among visible rows.
    pub tag_distinct: i64,
    /// Tags used by exactly one visible row.
    pub tag_singletons: i64,
    /// Visible rows carrying no tag.
    pub rows_untagged: i64,

    /// Memory-target edges named by visible rows (source-scoped, as in `memory_stats`).
    pub edges: i64,
    /// `edges / rows`; `None` when there are no visible rows (an empty ratio is not zero).
    pub edges_per_row: Option<f64>,
    /// Visible broken `[[…]]` targets.
    pub dangling: i64,

    /// Every number that could not be computed, named with its reason — never silently omitted.
    pub suppressed: Vec<String>,
}

/// A percentage, or `None` when the denominator is empty (an empty ratio is not zero).
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

/// Compute the health surface as ONE READER sees it.
pub fn compute_seen(conn: &Connection, reader: &Reader) -> Result<Health> {
    // Reuse the existing reader-scoped digest for the context numbers. That is deliberate: `rows`,
    // `body_chars`, tags, edges and dangling then cannot drift from `memory_stats`.
    let s = stats::compute_seen(conn, reader)?;
    let (pred, pred_binds) = reader.sql();

    // log_chars: the same eight prefixes the issue fixed. `GLOB`, not `LIKE`: `LIKE` is ASCII
    // case-insensitive in SQLite, so `ALERTS-x` would have counted as `alerts-x`; `GLOB` is literal.
    // Each pattern is a bound parameter so the prefix list stays one constant.
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

    // Woven, in both directions. `memory_link` has no reader predicate of its own, so an inbound
    // edge is visible only when BOTH ends are: the target is a visible row, and the SOURCE is a row
    // this reader may see. Without that second half, a private row's link to a public row would
    // silently raise the public row's `woven` — a one-bit disclosure of an edge the reader cannot
    // read (this is why `stats.linked` is not reused here; its orphan subquery has the same leak).
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
    // The outbound half is both-ends too: naming a target key is not an edge this reader can
    // follow unless the target is a visible memory row. This keeps `woven` exactly the union of
    // the two direction numbers instead of letting an invisible target raise it.
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

    // Cold30: rows older than 30 days that have never been read. The population is reported even
    // when the ratio is suppressed.
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

    // Duplicate pairs are NOT computed here. `dupes::find_seen` costs one `vec::knn` query per
    // visible row plus `load_rows` materialising every body (measured 19,658 ms of a 19,624 ms
    // call on a 1922-row snapshot — the shape, not the constant). The aggregate reports `null` and
    // names `memory_dupes` as the exact finder; it must never print `0`, which would assert "no
    // duplicates" from a count that was never taken.
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
    fn the_log_prefix_list_is_the_eight_the_issue_fixed() {
        assert_eq!(
            LOG_CLASS_PREFIXES,
            &[
                "alerts-",
                "hygiene-",
                "recess-",
                "maintenance-",
                "pr-watch-",
                "dispatch-",
                "offsite-",
                "cluster-watch-",
            ]
        );
        for prefix in LOG_CLASS_PREFIXES {
            assert!(
                LOG_SHARE_DEFINITION.contains(prefix),
                "the definition must name `{prefix}`: {LOG_SHARE_DEFINITION}"
            );
        }
    }

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
