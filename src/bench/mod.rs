use std::path::Path;
use std::time::Instant;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::search::{self, MatchMode, Query};

pub const DEFAULT_EVAL_K: usize = 3;

pub const RECALL_TARGET: f64 = 0.8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub query: String,
    pub expect: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<usize>,
}

pub fn load_cases(path: &Path) -> Result<Vec<Case>> {
    let text = std::fs::read_to_string(path)?;
    let mut cases = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let case: Case = serde_json::from_str(line).map_err(|e| {
            Error::Config(format!(
                "{}:{}: invalid golden case: {e}",
                path.display(),
                i + 1
            ))
        })?;
        if case.expect.is_empty() {
            return Err(Error::Config(format!(
                "{}:{}: `expect` must name at least one key",
                path.display(),
                i + 1
            )));
        }
        cases.push(case);
    }
    if cases.is_empty() {
        return Err(Error::Config(format!(
            "{}: no golden cases (one JSON object per line)",
            path.display()
        )));
    }
    Ok(cases)
}

#[derive(Debug, Clone, Serialize)]
pub struct Eval {
    pub cases: usize,
    pub hits: usize,
    pub recall: f64,
    pub mrr: f64,
    pub k: usize,
}

impl Eval {
    pub fn passes(&self) -> bool {
        self.recall >= RECALL_TARGET
    }
}

fn case_query(case: &Case, k: usize) -> Query {
    Query {
        key: None,
        key_prefix: false,
        title: None,
        status: None,
        text: Some(case.query.clone()),
        pattern: None,
        mode: MatchMode::Literal,
        k,
        max_distance: None,
        semantic_only: false,
        expand: 0,
        tags: Vec::new(),
        min_chars: None,
        max_chars: None,
        created_after: None,
        created_before: None,
        updated_after: None,
        updated_before: None,
    }
}

pub fn evaluate(
    conn: &Connection,
    embedder: Option<&dyn Embedder>,
    cases: &[Case],
    k: usize,
) -> Result<Eval> {
    let mut hits = 0usize;
    let mut reciprocal_ranks = 0.0f64;
    for case in cases {
        let cutoff = case.k.unwrap_or(k).max(1);
        let query = case_query(case, cutoff);
        let bundle = search::run(
            conn,
            embedder,
            &crate::store::audience::Reader::everything(),
            &query,
        )?;
        if let Some(rank) = bundle
            .hits
            .iter()
            .position(|h| case.expect.contains(&h.key))
        {
            hits += 1;
            reciprocal_ranks += 1.0 / (rank + 1) as f64;
        }
    }
    let n = cases.len();
    let denom = n.max(1) as f64;
    Ok(Eval {
        cases: n,
        hits,
        recall: hits as f64 / denom,
        mrr: reciprocal_ranks / denom,
        k,
    })
}

pub const SWEEP_KS: [usize; 3] = [3, 5, 10];

pub fn sweep(
    conn: &Connection,
    embedder: Option<&dyn Embedder>,
    cases: &[Case],
    ks: &[usize],
) -> Result<Vec<Eval>> {
    let uniform: Vec<Case> = cases
        .iter()
        .map(|c| Case {
            k: None,
            ..c.clone()
        })
        .collect();
    let mut out = Vec::with_capacity(ks.len());
    for &k in ks {
        out.push(evaluate(conn, embedder, &uniform, k)?);
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub n: usize,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

pub fn summarise(mut samples: Vec<f64>) -> Sample {
    let n = samples.len();
    if n == 0 {
        return Sample {
            n: 0,
            mean_ms: 0.0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            max_ms: 0.0,
        };
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean_ms = samples.iter().sum::<f64>() / n as f64;
    let p95_index = ((n as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    Sample {
        n,
        mean_ms,
        p50_ms: samples[n / 2],
        p95_ms: samples[p95_index],
        max_ms: samples[n - 1],
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Timings {
    pub rows: i64,
    pub dim: usize,
    pub load_ms: f64,
    pub embed: Sample,
    pub search: Sample,
}

pub fn timings(
    conn: &Connection,
    embedder: &dyn Embedder,
    load_ms: f64,
    samples: &[String],
    queries: &[String],
    k: usize,
) -> Result<Timings> {
    let rows = crate::index::vec::count(conn)?;
    if let Some(first) = samples.first() {
        embedder.embed_one(first)?;
    }

    let mut embed_times = Vec::with_capacity(samples.len());
    for text in samples {
        let started = Instant::now();
        embedder.embed_one(text)?;
        embed_times.push(started.elapsed().as_secs_f64() * 1e3);
    }

    let mut search_times = Vec::with_capacity(queries.len());
    for text in queries {
        let query = case_query(
            &Case {
                query: text.clone(),
                expect: Vec::new(),
                k: None,
            },
            k.max(1),
        );
        let started = Instant::now();
        search::run(
            conn,
            Some(embedder),
            &crate::store::audience::Reader::everything(),
            &query,
        )?;
        search_times.push(started.elapsed().as_secs_f64() * 1e3);
    }

    Ok(Timings {
        rows,
        dim: embedder.dim(),
        load_ms,
        embed: summarise(embed_times),
        search: summarise(search_times),
    })
}

pub fn best(evals: &[Eval]) -> Option<&Eval> {
    evals.iter().max_by(|a, b| {
        a.recall
            .partial_cmp(&b.recall)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.mrr
                    .partial_cmp(&b.mrr)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(query: &str, expect: &[&str]) -> Case {
        Case {
            query: query.to_string(),
            expect: expect.iter().map(|s| s.to_string()).collect(),
            k: None,
        }
    }

    #[test]
    fn case_is_an_accept_set_not_an_order() {
        let c = case("q", &["a", "b"]);
        assert!(c.expect.contains(&"b".to_string()));
        assert_eq!(c.k, None, "an absent per-case k defers to the caller");
    }

    #[test]
    fn summarise_reports_percentiles() {
        let s = summarise(vec![10.0, 20.0, 30.0, 40.0, 50.0]);
        assert_eq!(s.n, 5);
        assert_eq!(s.mean_ms, 30.0);
        assert_eq!(s.p50_ms, 30.0);
        assert_eq!(s.p95_ms, 50.0);
        assert_eq!(s.max_ms, 50.0);
    }

    #[test]
    fn summarise_of_nothing_is_zeroed() {
        let s = summarise(Vec::new());
        assert_eq!(s.n, 0);
        assert_eq!(s.mean_ms, 0.0);
    }

    #[test]
    fn best_prefers_recall_then_mrr() {
        let evals = vec![
            Eval {
                cases: 10,
                hits: 8,
                recall: 0.8,
                mrr: 0.6,
                k: 3,
            },
            Eval {
                cases: 10,
                hits: 8,
                recall: 0.8,
                mrr: 0.7,
                k: 5,
            },
            Eval {
                cases: 10,
                hits: 7,
                recall: 0.7,
                mrr: 0.9,
                k: 10,
            },
        ];
        let winner = best(&evals).expect("a best");
        assert_eq!(winner.k, 5, "higher MRR breaks the recall tie");
        assert!(best(&[]).is_none());
    }

    #[test]
    fn eval_passes_only_above_the_recall_gate() {
        let mut eval = Eval {
            cases: 10,
            hits: 8,
            recall: 0.8,
            mrr: 0.0,
            k: 3,
        };
        assert!(eval.passes());
        eval.recall = 0.79;
        assert!(!eval.passes());
    }
}
