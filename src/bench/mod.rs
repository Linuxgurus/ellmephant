//! P10 — turn the SPEC §13 estimates into numbers, and tune retrieval (PLAN §6 P10).
//!
//! Two instruments live here. [`timings`] measures the real load/embed/search latency against
//! a live store, and [`evaluate`] scores a *golden* query set with recall@k and MRR. [`sweep`]
//! walks the result count. Everything takes an [`Embedder`] trait object, so the whole module
//! runs offline with the deterministic [`HashEmbedder`](crate::embed::HashEmbedder).

use std::path::Path;
use std::time::Instant;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::search::{self, Query};

/// The cutoff the golden set is scored at unless it names its own (PLAN §6 P10).
pub const DEFAULT_EVAL_K: usize = 3;

/// Recall@[`DEFAULT_EVAL_K`] a golden set must clear to count as a pass (PLAN §6 P10).
pub const RECALL_TARGET: f64 = 0.8;

/// One golden query: a natural question and the keys that count as a hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    /// The user's question.
    pub query: String,
    /// Accept-set of keys — any one landing in the top `k` is a hit (not an order).
    pub expect: Vec<String>,
    /// Optional per-case cutoff; the caller's `k` applies when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<usize>,
}

/// Read a JSONL golden set: one [`Case`] per line, blank lines and `#` comments ignored.
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

/// The score of one golden-set run.
#[derive(Debug, Clone, Serialize)]
pub struct Eval {
    /// Golden cases scored.
    pub cases: usize,
    /// Cases with an expected key in the top `k`.
    pub hits: usize,
    /// `hits / cases` (the recall@k PLAN §6 P10 gates on).
    pub recall: f64,
    /// Mean reciprocal rank of the first expected key (0 for a miss).
    pub mrr: f64,
    /// The cutoff applied where a case did not name its own.
    pub k: usize,
}

impl Eval {
    /// Whether this run clears the agreed recall gate.
    pub fn passes(&self) -> bool {
        self.recall >= RECALL_TARGET
    }
}

/// The semantic query a golden case runs: the phrase against both embedded fields.
fn case_query(case: &Case, k: usize) -> Query {
    Query {
        text: Some(case.query.clone()),
        k,
        max_distance: None,
        expand: 0,
        tags: Vec::new(),
        min_chars: None,
        max_chars: None,
        coherent: None,
    }
}

/// Score `cases` against the live store: run each query and look for an expected key in the
/// top `k`.
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
        let bundle = search::run(conn, embedder, &query)?;
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

/// The default result counts to sweep.
pub const SWEEP_KS: [usize; 3] = [3, 5, 10];

/// Walk every `k`, scoring the golden set for each (PLAN §6 P10: "sweep k"). Cases with their
/// own `k` are cleared so the swept cutoff applies uniformly.
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

/// A latency distribution: `n` samples summarised, all in milliseconds.
#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    /// Samples taken.
    pub n: usize,
    /// Arithmetic mean.
    pub mean_ms: f64,
    /// Median.
    pub p50_ms: f64,
    /// 95th percentile.
    pub p95_ms: f64,
    /// Worst sample.
    pub max_ms: f64,
}

/// Summarise samples (milliseconds); empty input yields a zeroed [`Sample`].
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

/// Measured latency and the scale it was measured at (SPEC §13, replaced with numbers).
#[derive(Debug, Clone, Serialize)]
pub struct Timings {
    /// Indexed title vectors (the `N rows` the numbers are reported at).
    pub rows: i64,
    /// Embedding dimension.
    pub dim: usize,
    /// Time to load the encoder (measured by the caller), in milliseconds.
    pub load_ms: f64,
    /// Document-embedding latency.
    pub embed: Sample,
    /// Search latency (query embed + KNN + hydrate).
    pub search: Sample,
}

/// Measure embed and search latency. `samples` are embedded as documents; `queries` are run
/// through [`search::run`]. A warmup embed is issued first so the first sample does not carry
/// one-time costs (lazy allocations, thread-pool spin-up).
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
        search::run(conn, Some(embedder), &query)?;
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

/// The best-scoring configuration in a sweep, by recall then MRR.
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
