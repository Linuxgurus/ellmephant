//! Duplicate-candidate audit: pairs of memories whose document embeddings sit unusually close
//! (the finder for the §9 "merge redundant memories" rule).
//!
//! A *finder*, not a judge — and it says so with a [`Class`]. A cosine distance is a fact about an
//! embedder; "these two rows are the same thing" is a judgement about the world. The finder used to
//! return the distance alone, so a dated series (the same measurement taken three days running) and a
//! genuine duplicate looked identical to every caller. Each pair now carries the class of the *pair*,
//! decided by a rule that can be read and disagreed with (soma/ellmephant#219).
//!
//! The embedded document is `key` + title + body, so merely-related memories cluster here too.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde::Serialize;

use crate::error::Result;
use crate::index::vec::{self, Knn};

/// Default cosine-distance ceiling. Tight on purpose: a loose ceiling returns same-topic
/// neighbours, not duplicates.
pub const DEFAULT_MAX_DISTANCE: f32 = 0.05;

/// Nearest neighbours examined per memory (self excluded). A duplicate is the *nearest* row,
/// so a handful is plenty; a pair missed at one end is still seen from the other.
const NEIGHBOURS: usize = 3;

/// How alike two bodies must be, by token overlap, to be called a duplicate. Deliberately high: a
/// close embedding is not evidence, and the cost of a false `duplicate` is a merge that destroys a row.
const DUPLICATE_OVERLAP: f64 = 0.85;

/// One candidate pair (keys and titles, no bodies).
#[derive(Debug, Clone, Serialize)]
pub struct DupePair {
    /// Lexicographically-smaller key, so a pair is emitted once.
    pub key_a: String,
    /// Title of `key_a`.
    pub title_a: String,
    /// The other key.
    pub key_b: String,
    /// Title of `key_b`.
    pub title_b: String,
    /// Cosine distance between the two documents.
    pub distance: f32,
    /// What KIND of pair this is. The judgement the finder used to leave to the caller — and the
    /// caller could not make it, because it was never given the bodies.
    pub class: Class,
}

/// What a candidate pair actually is.
///
/// ★ THE ORDER OF THE TESTS IS THE RULE: series before artifact before duplicate. A dated series can
/// have near-identical bodies (that is the point of a nightly measurement), and an artifact pair can
/// be a verbatim duplicate — but neither is redundant, so neither may reach a merge plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Near-identical content, no dated class, no recoverable source: a keeper/loser plan is safe.
    Duplicate,
    /// Both keys end in the same shape of date or clock marker: a series, never a merge candidate.
    Series,
    /// A verbatim capture (diff, issue body, log dump) whose source is recoverable: collapsing is
    /// safe only into ONE row that cites the source.
    Artifact,
    /// Close by embedding, different in content: the false-positive class that made the finder
    /// untrustworthy. Report it and move on.
    Distinct,
}

impl Class {
    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Duplicate => "duplicate",
            Class::Series => "series",
            Class::Artifact => "artifact",
            Class::Distinct => "distinct",
        }
    }
}

/// One step of a merge plan: fold `absorb` into `keep`.
#[derive(Debug, Clone, Serialize)]
pub struct PlanEntry {
    /// The row that survives.
    pub keep: String,
    /// The row whose lines move into `keep` — and which a human then deletes.
    pub absorb: String,
    /// Why `keep` is the keeper. A plan that does not say why is an order.
    pub reason: String,
}

/// A pair `--plan` will not order a merge for, with the reason.
#[derive(Debug, Clone, Serialize)]
pub struct Refusal {
    pub key_a: String,
    pub key_b: String,
    pub class: Class,
    pub reason: String,
}

/// The merge plan over the candidate pairs.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MergePlan {
    pub steps: Vec<PlanEntry>,
    /// Every non-`duplicate` pair, with why it is not a merge. A series pair lands here rather than
    /// silently vanishing: "refused" is the useful answer.
    pub refused: Vec<Refusal>,
}

/// Every pair of memories closer than `max_distance`, nearest first, capped at `limit`, each with
/// its class.
///
/// One KNN pass per memory; pairs are deduplicated (`a`,`b` == `b`,`a`) and no memory is its
/// own neighbour.
pub fn find(conn: &Connection, max_distance: f32, limit: usize) -> Result<Vec<DupePair>> {
    find_seen(
        conn,
        &crate::store::audience::Reader::everything(),
        max_distance,
        limit,
    )
}

/// The same finder over the rows one reader may see. A private row must not be paired with a public
/// neighbour: the pair itself would name it, and the distance would describe its content.
pub fn find_seen(
    conn: &Connection,
    reader: &crate::store::audience::Reader,
    max_distance: f32,
    limit: usize,
) -> Result<Vec<DupePair>> {
    let rows = load_rows(conn, reader)?;
    if rows.len() < 2 {
        return Ok(Vec::new());
    }
    // +1 so that after dropping the self-match we still have `NEIGHBOURS` others.
    let k = (NEIGHBOURS + 1).min(rows.len());

    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    let mut pairs: Vec<DupePair> = Vec::new();
    for &id in rows.keys() {
        let Some(vector) = vec::get(conn, id)? else {
            continue; // a row with no vector (should not happen) is simply skipped
        };
        for (other, distance) in vec::knn(
            conn,
            &vector,
            &Knn {
                k,
                max_distance: None,
            },
            Some("memory"),
        )? {
            if other == id || distance > max_distance {
                continue;
            }
            let (lo, hi) = if id < other { (id, other) } else { (other, id) };
            if !seen.insert((lo, hi)) {
                continue;
            }
            let (Some((key_a, title_a, body_a)), Some((key_b, title_b, body_b))) =
                (rows.get(&lo), rows.get(&hi))
            else {
                continue;
            };
            pairs.push(DupePair {
                key_a: key_a.clone(),
                title_a: title_a.clone(),
                key_b: key_b.clone(),
                title_b: title_b.clone(),
                distance,
                class: classify(key_a, body_a, key_b, body_b),
            });
        }
    }
    pairs.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    pairs.truncate(limit);
    Ok(pairs)
}

/// Find the pairs one reader may see and build the merge plan from them.
pub fn plan_seen(
    conn: &Connection,
    reader: &crate::store::audience::Reader,
    max_distance: f32,
    limit: usize,
) -> Result<MergePlan> {
    let pairs = find_seen(conn, reader, max_distance, limit)?;
    plan(conn, &pairs)
}

/// Build the merge plan: a keeper/loser step for every `duplicate`, and a named refusal for
/// everything else. The keeper is chosen by evidence the store already keeps, never by distance.
pub fn plan(conn: &Connection, pairs: &[DupePair]) -> Result<MergePlan> {
    let mut out = MergePlan::default();
    for pair in pairs {
        match pair.class {
            Class::Duplicate => {
                let (keep, absorb, reason) = keeper(conn, &pair.key_a, &pair.key_b)?;
                out.steps.push(PlanEntry {
                    keep,
                    absorb,
                    reason,
                });
            }
            Class::Series => out.refused.push(Refusal {
                key_a: pair.key_a.clone(),
                key_b: pair.key_b.clone(),
                class: pair.class,
                reason: "a dated series: each row is the only record of its day, so merging them \
                         destroys the series to save nothing. If the newest supersedes the older, \
                         delete the older and leave a pointer — do not merge."
                    .into(),
            }),
            Class::Artifact => out.refused.push(Refusal {
                key_a: pair.key_a.clone(),
                key_b: pair.key_b.clone(),
                class: pair.class,
                reason: "a verbatim capture: collapsing is safe only into ONE row that CITES the \
                         source (SHA, issue or URL). Cite it first, then merge."
                    .into(),
            }),
            Class::Distinct => out.refused.push(Refusal {
                key_a: pair.key_a.clone(),
                key_b: pair.key_b.clone(),
                class: pair.class,
                reason: "close by embedding, different in content: a false positive, not a merge \
                         candidate."
                    .into(),
            }),
        }
    }
    Ok(out)
}

/// Pick the keeper by what the store already knows: inbound references, then reads, then age.
/// The tie-break is the smaller key, so the plan is stable across runs.
fn keeper(conn: &Connection, a: &str, b: &str) -> Result<(String, String, String)> {
    let (edges_a, visits_a, created_a) = evidence(conn, a)?;
    let (edges_b, visits_b, created_b) = evidence(conn, b)?;
    let keep_a = if (edges_a, visits_a) != (edges_b, visits_b) {
        (edges_a, visits_a) > (edges_b, visits_b)
    } else if created_a != created_b {
        // The earlier row is the original; the later one is more likely the copy.
        created_a < created_b
    } else {
        a < b
    };
    let (keep, absorb, reason) = if keep_a {
        (
            a,
            b,
            if edges_a != edges_b {
                format!("more inbound references ({edges_a} vs {edges_b})")
            } else if visits_a != visits_b {
                format!("more reads ({visits_a} vs {visits_b} visits)")
            } else if created_a != created_b {
                format!("older ({created_a} vs {created_b})")
            } else {
                "tie: the lexicographically smaller key".to_string()
            },
        )
    } else {
        (
            b,
            a,
            if edges_b != edges_a {
                format!("more inbound references ({edges_b} vs {edges_a})")
            } else if visits_b != visits_a {
                format!("more reads ({visits_b} vs {visits_a} visits)")
            } else if created_b != created_a {
                format!("older ({created_b} vs {created_a})")
            } else {
                "tie: the lexicographically smaller key".to_string()
            },
        )
    };
    Ok((keep.to_string(), absorb.to_string(), reason))
}

/// `(inbound memory edges, visits, created)` for one row.
fn evidence(conn: &Connection, key: &str) -> Result<(i64, i64, String)> {
    let (visits, created): (i64, String) = conn
        .query_row(
            "SELECT visits, created FROM memory WHERE key = ?1",
            rusqlite::params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, String::new()));
    let edges: i64 = conn
        .query_row(
            "SELECT count(*) FROM memory_link WHERE to_key = ?1 AND to_type = 'memory'",
            rusqlite::params![key],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok((edges, visits, created))
}

/// Classify one pair. Body-first: a key is a hint, never the test.
fn classify(key_a: &str, body_a: &str, key_b: &str, body_b: &str) -> Class {
    // ★ A SERIES FIRST. Two nightly measurements can be byte-identical; the date is what makes them
    // different records, so it must be asked before "these bodies are the same".
    if let (Some(shape_a), Some(shape_b)) = (key_marker(key_a), key_marker(key_b))
        && shape_a == shape_b
    {
        return Class::Series;
    }
    if is_artifact(key_a, body_a) && is_artifact(key_b, body_b) {
        return Class::Artifact;
    }
    if same_content(body_a, body_b) {
        return Class::Duplicate;
    }
    Class::Distinct
}

/// The shape of the date or clock marker a key ends in, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    /// `YYYY-MM-DD` at the end.
    Day,
    /// `HHMMz` (optionally `-HHMMz`) at the end — the alert-ledger clock shape.
    Clock,
}

fn key_marker(key: &str) -> Option<Marker> {
    // Clock first: `alerts-2026-11-03-0538z` ends in a clock, and its day is context, not identity.
    // Keys are slugs (`[a-z0-9-]+`), so byte slicing is safe.
    if let Some(stem) = key.strip_suffix('z') {
        let digits = stem.len() >= 5 && stem[stem.len() - 4..].bytes().all(|b| b.is_ascii_digit());
        if digits && stem.as_bytes()[stem.len() - 5] == b'-' {
            return Some(Marker::Clock);
        }
    }
    if key.len() >= 10 && is_day(&key[key.len() - 10..]) {
        return Some(Marker::Day);
    }
    None
}

/// `YYYY-MM-DD`, shape only (a real calendar date is not required).
fn is_day(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7) || c.is_ascii_digit())
}

/// Is this body a verbatim capture whose source is recoverable?
///
/// Two questions, both required: it must LOOK like a capture (a diff, or a body that is mostly one
/// fenced block) and it must NAME where the original lives (a SHA, a commit/issue URL, or `issue #N`).
/// A lesson that quotes a SHA is not an artifact; a dump with no address is not safely collapsible.
fn is_artifact(key: &str, body: &str) -> bool {
    let capture = body.starts_with("diff --git")
        || body.contains("\n@@ ") && body.contains("\n+++ ")
        || fenced_share(body) >= 0.6;
    if !capture {
        return false;
    }
    let addressed = body.contains("/issues/")
        || body.contains("/commit/")
        || body.contains("issue #")
        || body.contains("Issue #")
        || has_sha(body);
    // A `pr<number>-…` key is itself an address into a forge, when the body is a capture.
    addressed || is_pr_key(key)
}

/// Share of non-empty body lines that sit inside a fenced code block.
fn fenced_share(body: &str) -> f64 {
    let mut total = 0usize;
    let mut inside = 0usize;
    let mut fenced = false;
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            inside += 1;
        } else if fenced {
            inside += 1;
        }
    }
    if total == 0 {
        return 0.0;
    }
    inside as f64 / total as f64
}

/// `pr145-…` — a forge artifact's key.
fn is_pr_key(key: &str) -> bool {
    let Some(rest) = key.strip_prefix("pr") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest.as_bytes().get(digits) == Some(&b'-')
}

/// A 7–40 character hex run that is not merely a word: a git SHA.
fn has_sha(body: &str) -> bool {
    body.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| (7..=40).contains(&tok.len()) && tok.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Are these two bodies the same content, not merely nearby?
///
/// Normalised equality first (whitespace and punctuation do not make a difference), then a
/// line-containment test (the shorter body's lines all appear in the longer), then token overlap.
fn same_content(a: &str, b: &str) -> bool {
    let na = normalise(a);
    let nb = normalise(b);
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    if na == nb {
        return true;
    }
    // A verbatim append: every line of the shorter body is already carried by the longer one. The
    // length floor is load-bearing: without it a one-line "ok" is carried by every row that says it.
    let la: Vec<&str> = a.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let lb: Vec<&str> = b.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let (short, long) = if la.len() <= lb.len() {
        (&la, &lb)
    } else {
        (&lb, &la)
    };
    let short_chars: usize = short.iter().map(|l| l.chars().count()).sum();
    if short_chars >= 40 && short.iter().all(|line| long.contains(line)) {
        return true;
    }
    // Token overlap on a body with only a handful of words is noise, not sameness.
    let tokens = na.split(' ').count().min(nb.split(' ').count());
    tokens >= 8 && overlap(&na, &nb) >= DUPLICATE_OVERLAP
}

/// Lowercase alphanumeric words, single-spaced.
fn normalise(body: &str) -> String {
    body.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Jaccard overlap of the token sets.
fn overlap(a: &str, b: &str) -> f64 {
    let sa: HashSet<&str> = a.split(' ').collect();
    let sb: HashSet<&str> = b.split(' ').collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let shared = sa.intersection(&sb).count() as f64;
    shared / sa.union(&sb).count() as f64
}

/// `id -> (key, title, body)` for every memory the reader may see.
fn load_rows(
    conn: &Connection,
    reader: &crate::store::audience::Reader,
) -> Result<HashMap<i64, (String, String, String)>> {
    let (predicate, binds) = reader.sql();
    let mut stmt = conn.prepare(&format!(
        "SELECT id, key, title, body FROM memory WHERE {predicate}"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                (
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ),
            ))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

/// Positional wire for the pair list (mirrors the other list-shaped reads): a `cols` header and
/// bare-array rows. `distance` is rounded to 4 decimals.
pub fn to_wire_json(pairs: &[DupePair]) -> Result<String> {
    let rows: Vec<serde_json::Value> = pairs
        .iter()
        .map(|p| {
            serde_json::json!([
                p.key_a,
                p.title_a,
                p.key_b,
                p.title_b,
                ((p.distance as f64) * 10_000.0).round() / 10_000.0,
                p.class.as_str()
            ])
        })
        .collect();
    Ok(format!(
        "{{\"cols\":[\"key_a\",\"title_a\",\"key_b\",\"title_b\",\"distance\",\"class\"],\
         \"count\":{},\"pairs\":{}}}",
        pairs.len(),
        crate::wire::rows_array(&rows)
    ))
}

/// Positional wire for [`MergePlan`]: the steps to take, and the pairs `--plan` refuses with the
/// reason. Both tables are positional, like every other list-shaped read.
pub fn plan_to_wire_json(plan: &MergePlan) -> Result<String> {
    let steps: Vec<serde_json::Value> = plan
        .steps
        .iter()
        .map(|e| serde_json::json!([e.keep, e.absorb, e.reason]))
        .collect();
    let refused: Vec<serde_json::Value> = plan
        .refused
        .iter()
        .map(|r| serde_json::json!([r.key_a, r.key_b, r.class.as_str(), r.reason]))
        .collect();
    Ok(format!(
        "{{\"cols\":[\"keep\",\"absorb\",\"reason\"],\"count\":{},\"plan\":{},\
         \"refused_cols\":[\"key_a\",\"key_b\",\"class\",\"reason\"],\"refused_count\":{},\
         \"refused\":{}}}",
        plan.steps.len(),
        crate::wire::rows_array(&steps),
        plan.refused.len(),
        crate::wire::rows_array(&refused)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_is_a_positional_pair_table() {
        let pairs = vec![DupePair {
            key_a: "a".into(),
            title_a: "A".into(),
            key_b: "b".into(),
            title_b: "B".into(),
            distance: 0.012_345,
            class: Class::Duplicate,
        }];
        let wire = to_wire_json(&pairs).unwrap();
        assert!(wire.starts_with(
            "{\"cols\":[\"key_a\",\"title_a\",\"key_b\",\"title_b\",\"distance\",\"class\"]"
        ));
        assert!(wire.contains("\"count\":1"));
        assert!(
            wire.contains("[\"a\",\"A\",\"b\",\"B\",0.0123,\"duplicate\"]"),
            "{wire}"
        );
        assert!(to_wire_json(&[]).unwrap().contains("\"pairs\":[]"));
    }

    #[test]
    fn a_dated_series_is_never_a_duplicate_even_when_the_bodies_match() {
        let body = "offsite backup verified: 606 ids, 0 unreconstructable";
        assert_eq!(
            classify("offsite-2026-11-05", body, "offsite-2026-11-06", body),
            Class::Series
        );
        assert_eq!(
            classify(
                "alerts-2026-11-03-0538z",
                body,
                "alerts-2026-11-03-0638z",
                body
            ),
            Class::Series
        );
        // Different shapes are not a series: a day and a clock are different records.
        assert_ne!(
            classify("offsite-2026-11-05", body, "alerts-2026-11-03-0538z", body),
            Class::Series
        );
    }

    #[test]
    fn the_same_body_twice_is_a_duplicate() {
        let body = "the turn boundary is offered once per wake; a miss is not a failure.";
        assert_eq!(
            classify(
                "spine-issue-52-turn-boundary",
                body,
                "spine-52-turn-boundary-for-stet",
                body
            ),
            Class::Duplicate
        );
    }

    #[test]
    fn a_diff_that_names_its_source_is_an_artifact() {
        let body =
            "diff --git a/src/x.rs b/src/x.rs\n@@ -1,2 +1,3 @@\n-old\n+new\nsee commit 643eeb8";
        assert_eq!(
            classify("pr192-current-diff-1-of-4", body, "pr192-head-1-of-3", body),
            Class::Artifact
        );
    }

    #[test]
    fn a_lesson_that_quotes_a_sha_is_not_an_artifact() {
        assert_ne!(
            classify(
                "a-lesson",
                "commit 643eeb8 taught me that a gate must have a door.",
                "another-lesson",
                "commit 643eeb8 taught me that a gate must have a door."
            ),
            Class::Artifact
        );
    }

    #[test]
    fn close_but_different_is_distinct() {
        assert_eq!(
            classify(
                "offsite-retention",
                "backups are verified by opening a copy, never by the copy existing.",
                "offsite-schedule",
                "the offsite job runs at 04:30 and reports the id count it verified."
            ),
            Class::Distinct
        );
    }

    #[test]
    fn the_plan_table_names_the_keeper_and_the_refusals() {
        let plan = MergePlan {
            steps: vec![PlanEntry {
                keep: "keeper".into(),
                absorb: "loser".into(),
                reason: "more reads (9 vs 2 visits)".into(),
            }],
            refused: vec![Refusal {
                key_a: "offsite-2026-11-05".into(),
                key_b: "offsite-2026-11-06".into(),
                class: Class::Series,
                reason: "a dated series".into(),
            }],
        };
        let wire = plan_to_wire_json(&plan).unwrap();
        assert!(wire.contains("\"count\":1"), "{wire}");
        assert!(
            wire.contains("[\"keeper\",\"loser\",\"more reads (9 vs 2 visits)\"]"),
            "{wire}"
        );
        assert!(wire.contains("\"refused_count\":1"), "{wire}");
        assert!(wire.contains("\"series\""), "{wire}");
    }
}
