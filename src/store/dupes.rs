use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::Connection;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::index::vec::{self, Knn};

pub const DEFAULT_MAX_DISTANCE: f32 = 0.05;

const NEIGHBOURS: usize = 3;

const DUPLICATE_OVERLAP: f64 = 0.85;

#[derive(Debug, Clone, Serialize)]
pub struct DupePair {
    pub key_a: String,
    pub title_a: String,
    pub key_b: String,
    pub title_b: String,
    pub distance: f32,
    pub class: Class,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Duplicate,
    Series,
    Artifact,
    Distinct,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Duplicate => "duplicate",
            Class::Series => "series",
            Class::Artifact => "artifact",
            Class::Distinct => "distinct",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanEntry {
    pub keep: String,
    pub absorb: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Refusal {
    pub key_a: String,
    pub key_b: String,
    pub class: Class,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MergePlan {
    pub steps: Vec<PlanEntry>,
    pub refused: Vec<Refusal>,
}

pub fn find(conn: &Connection, max_distance: f32, limit: usize) -> Result<Vec<DupePair>> {
    find_seen(
        conn,
        &crate::store::audience::Reader::everything(),
        max_distance,
        limit,
    )
}

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
    let k = (NEIGHBOURS + 1).min(rows.len());

    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    let mut pairs: Vec<DupePair> = Vec::new();
    for &id in rows.keys() {
        let Some(vector) = vec::get(conn, id)? else {
            continue;
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

pub fn plan_seen(
    conn: &Connection,
    reader: &crate::store::audience::Reader,
    max_distance: f32,
    limit: usize,
) -> Result<MergePlan> {
    let pairs = find_seen(conn, reader, max_distance, limit)?;
    plan(conn, &pairs)
}

pub fn plan(conn: &Connection, pairs: &[DupePair]) -> Result<MergePlan> {
    let mut out = MergePlan::default();

    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut parent: Vec<usize> = Vec::new();
    for pair in pairs {
        if pair.class != Class::Duplicate {
            out.refused.push(refusal(pair, pair.class));
            continue;
        }
        let a = slot(&mut ids, &mut parent, &pair.key_a);
        let b = slot(&mut ids, &mut parent, &pair.key_b);
        let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    }

    let mut components: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (key, &id) in &ids {
        let r = root(&mut parent, id);
        components.entry(r).or_default().push(key.clone());
    }
    for mut members in components.into_values() {
        if members.len() < 2 {
            continue;
        }
        members.sort();
        let mut keep = members[0].clone();
        for m in &members[1..] {
            keep = keeper(conn, &keep, m)?.0;
        }
        let cluster = members.len();
        for m in &members {
            if *m == keep {
                continue;
            }
            let (_, _, mut reason) = keeper(conn, &keep, m)?;
            if cluster > 2 {
                reason.push_str(&format!(" (one keeper for a cluster of {cluster})"));
            }
            out.steps.push(PlanEntry {
                keep: keep.clone(),
                absorb: m.clone(),
                reason,
            });
        }
    }
    out.steps
        .sort_by(|a, b| a.keep.cmp(&b.keep).then_with(|| a.absorb.cmp(&b.absorb)));
    out.refused
        .sort_by(|a, b| a.key_a.cmp(&b.key_a).then_with(|| a.key_b.cmp(&b.key_b)));
    Ok(out)
}

fn slot(ids: &mut HashMap<String, usize>, parent: &mut Vec<usize>, key: &str) -> usize {
    if let Some(&i) = ids.get(key) {
        return i;
    }
    let i = parent.len();
    parent.push(i);
    ids.insert(key.to_string(), i);
    i
}

fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

fn refusal(pair: &DupePair, class: Class) -> Refusal {
    let reason = match class {
        Class::Series => "a dated series: each row is the only record of its day, so merging them \
                         destroys the series to save nothing. If the newest supersedes the older, \
                         delete the older and leave a pointer — do not merge."
            .to_string(),
        Class::Artifact => "a verbatim capture: collapsing is safe only into ONE row that CITES the \
                            source (SHA, issue or URL). Cite it first, then merge — and never merge \
                            a capture into a lesson that already carries it."
            .to_string(),
        Class::Distinct => "close by embedding, different in content: a false positive, not a merge \
                            candidate."
            .to_string(),
        Class::Duplicate => "a duplicate: merged into its component's keeper".to_string(),
    };
    Refusal {
        key_a: pair.key_a.clone(),
        key_b: pair.key_b.clone(),
        class,
        reason,
    }
}

fn keeper(conn: &Connection, a: &str, b: &str) -> Result<(String, String, String)> {
    let (edges_a, visits_a, created_a) = evidence(conn, a)?;
    let (edges_b, visits_b, created_b) = evidence(conn, b)?;
    let keep_a = if (edges_a, visits_a) != (edges_b, visits_b) {
        (edges_a, visits_a) > (edges_b, visits_b)
    } else if created_a != created_b {
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

fn evidence(conn: &Connection, key: &str) -> Result<(i64, i64, String)> {
    let (visits, created): (i64, String) = conn
        .query_row(
            "SELECT visits, created FROM memory WHERE key = ?1",
            rusqlite::params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound(key.to_string()),
            other => Error::Db(other),
        })?;
    let edges: i64 = conn.query_row(
        "SELECT count(*) FROM memory_link WHERE to_key = ?1 AND to_type = 'memory'",
        rusqlite::params![key],
        |r| r.get(0),
    )?;
    Ok((edges, visits, created))
}

fn classify(key_a: &str, body_a: &str, key_b: &str, body_b: &str) -> Class {
    if let (Some(shape_a), Some(shape_b)) = (key_marker(key_a), key_marker(key_b))
        && shape_a == shape_b
    {
        return Class::Series;
    }
    let (art_a, art_b) = (is_artifact(key_a, body_a), is_artifact(key_b, body_b));
    if art_a && art_b {
        return Class::Artifact;
    }
    if art_a != art_b && same_content(body_a, body_b) {
        return Class::Artifact;
    }
    if same_content(body_a, body_b) {
        return Class::Duplicate;
    }
    Class::Distinct
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Day,
    Clock,
}

fn key_marker(key: &str) -> Option<Marker> {
    if let Some(stem) = key.strip_suffix('z') {
        let tail = stem.len().checked_sub(4).and_then(|i| stem.get(i..));
        let dash = stem.len().checked_sub(5).and_then(|i| stem.get(i..i + 1));
        if let (Some(tail), Some("-")) = (tail, dash)
            && tail.bytes().all(|b| b.is_ascii_digit())
        {
            return Some(Marker::Clock);
        }
    }
    if let Some(day) = key.len().checked_sub(10).and_then(|i| key.get(i..))
        && is_day(day)
    {
        return Some(Marker::Day);
    }
    None
}

fn is_day(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7) || c.is_ascii_digit())
}

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
    addressed || is_pr_key(key)
}

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

fn is_pr_key(key: &str) -> bool {
    let Some(rest) = key.strip_prefix("pr") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest.as_bytes().get(digits) == Some(&b'-')
}

fn has_sha(body: &str) -> bool {
    body.split(|c: char| !c.is_ascii_alphanumeric()).any(|tok| {
        (7..=40).contains(&tok.len())
            && tok.bytes().all(|b| b.is_ascii_hexdigit())
            && tok.bytes().any(|b| b.is_ascii_digit())
            && tok.bytes().any(|b| b.is_ascii_alphabetic())
    })
}

fn same_content(a: &str, b: &str) -> bool {
    let na = normalise(a);
    let nb = normalise(b);
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    if na == nb {
        return true;
    }
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
    let tokens = na.split(' ').count().min(nb.split(' ').count());
    tokens >= 8 && overlap(&na, &nb) >= DUPLICATE_OVERLAP
}

fn normalise(body: &str) -> String {
    body.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn overlap(a: &str, b: &str) -> f64 {
    let sa: HashSet<&str> = a.split(' ').collect();
    let sb: HashSet<&str> = b.split(' ').collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let shared = sa.intersection(&sb).count() as f64;
    shared / sa.union(&sb).count() as f64
}

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
        let wire = crate::wire::dupes::to_wire_json(&pairs).unwrap();
        assert!(wire.starts_with(
            "{\"cols\":[\"key_a\",\"title_a\",\"key_b\",\"title_b\",\"distance\",\"class\"]"
        ));
        assert!(wire.contains("\"count\":1"));
        assert!(
            wire.contains("[\"a\",\"A\",\"b\",\"B\",0.0123,\"duplicate\"]"),
            "{wire}"
        );
        assert!(
            crate::wire::dupes::to_wire_json(&[])
                .unwrap()
                .contains("\"pairs\":[]")
        );
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
        assert_ne!(
            classify("offsite-2026-11-05", body, "alerts-2026-11-03-0538z", body),
            Class::Series
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
    fn a_capture_beside_the_lesson_that_carries_it_is_an_artifact() {
        let capture = "```text\nerror: the gate refused\ncommit 643eeb8\n```\n";
        let lesson = format!(
            "{capture}\nThe lesson: read the log before inventing a cause.\n\
             A cache push is not a test.\n\
             A re-run that claims a cause is indistinguishable from a fix.\n"
        );
        assert!(is_artifact("pr192-gate-log", capture));
        assert!(
            !is_artifact("a-lesson", &lesson),
            "the lesson is mostly prose, so it is not itself a capture"
        );
        assert_eq!(
            classify("pr192-gate-log", capture, "a-lesson", &lesson),
            Class::Artifact
        );
    }

    #[test]
    fn hex_shaped_words_are_not_addresses() {
        assert!(!has_sha("defaced accede"));
        assert!(!has_sha("the id is 1234567"), "a number is not an address");
        assert!(has_sha("see 643eeb8"), "a mixed-alphabet short SHA is one");
        assert!(has_sha(&"a1".repeat(20)), "a full 40-char SHA is one");
    }

    #[test]
    fn key_markers_do_not_slice_through_a_char_boundary() {
        assert_eq!(key_marker("héllo-☃z"), None);
        assert_eq!(key_marker("日本-0538z"), Some(Marker::Clock));
        assert_eq!(key_marker("offsite-2026-11-05"), Some(Marker::Day));
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
        let wire = crate::wire::dupes::plan_to_wire_json(&plan).unwrap();
        assert!(wire.contains("\"count\":1"), "{wire}");
        assert!(
            wire.contains("[\"keeper\",\"loser\",\"more reads (9 vs 2 visits)\"]"),
            "{wire}"
        );
        assert!(wire.contains("\"refused_count\":1"), "{wire}");
        assert!(wire.contains("\"series\""), "{wire}");
    }
}
