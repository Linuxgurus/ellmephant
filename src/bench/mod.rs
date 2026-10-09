//! P10 — turn the SPEC §13 estimates into numbers, and tune retrieval (PLAN §6 P10).
//!
//! Two instruments live here. [`timings`] measures the real load/embed/search latency against
//! a live store, and [`evaluate`] scores a *golden* query set with recall@k and MRR. [`sweep`]
//! walks the query-time knobs (result count, RRF weight) and [`sweep_projections`] compares
//! the embed projections offline. Everything takes an [`Embedder`] trait object, so the whole
//! module runs offline with the deterministic [`HashEmbedder`](crate::embed::HashEmbedder).

use std::path::Path;
use std::time::Instant;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::embed::{Embedder, projection};
use crate::error::{Error, Result};
use crate::search::{self, DEFAULT_RRF_K, Mode, Query};
use crate::store::Memory;

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
    /// The RRF constant the run used.
    pub rrf_k: f64,
    /// The retrieval mode the run used.
    pub mode: String,
}

impl Eval {
    /// Whether this run clears the agreed recall gate.
    pub fn passes(&self) -> bool {
        self.recall >= RECALL_TARGET
    }
}

/// Score `cases` against the live store: run each query and look for an expected key in the
/// top `k`. `rrf_k` is threaded through the [`Query`] so a sweep can vary it.
pub fn evaluate(
    conn: &Connection,
    embedder: Option<&dyn Embedder>,
    cases: &[Case],
    k: usize,
    rrf_k: f64,
    mode: Mode,
) -> Result<Eval> {
    let mut hits = 0usize;
    let mut reciprocal_ranks = 0.0f64;
    for case in cases {
        let cutoff = case.k.unwrap_or(k).max(1);
        let query = Query {
            text: case.query.clone(),
            scope: None,
            k: cutoff,
            mode,
            max_distance: None,
            expand: 0,
            rrf_k,
        };
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
        rrf_k,
        mode: mode.as_str().to_string(),
    })
}

/// The default result counts to sweep.
pub const SWEEP_KS: [usize; 3] = [3, 5, 10];
/// RRF constants around the SPEC §10 default of 60.
pub const SWEEP_RRF_KS: [f64; 3] = [20.0, 60.0, 100.0];

/// Walk every `(k, rrf_k)` pair, scoring the golden set for each (PLAN §6 P10: "sweep k,
/// RRF weights"). Cases with their own `k` are cleared so the swept cutoff applies uniformly.
pub fn sweep(
    conn: &Connection,
    embedder: Option<&dyn Embedder>,
    cases: &[Case],
    ks: &[usize],
    rrf_ks: &[f64],
    mode: Mode,
) -> Result<Vec<Eval>> {
    let uniform: Vec<Case> = cases
        .iter()
        .map(|c| Case {
            k: None,
            ..c.clone()
        })
        .collect();
    let mut out = Vec::with_capacity(ks.len() * rrf_ks.len());
    for &k in ks {
        for &rrf_k in rrf_ks {
            out.push(evaluate(conn, embedder, &uniform, k, rrf_k, mode)?);
        }
    }
    Ok(out)
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
    /// Indexed vectors (the `N rows` the numbers are reported at).
    pub rows: i64,
    /// Embedding dimension.
    pub dim: usize,
    /// Time to load the encoder (measured by the caller), in milliseconds.
    pub load_ms: f64,
    /// Document-embedding latency.
    pub embed: Sample,
    /// Hybrid-search latency (query embed + both legs + fuse + hydrate).
    pub search: Sample,
}

/// Measure embed and hybrid-search latency. `samples` are embedded as documents; `queries`
/// are run through [`search::run`]. A warmup embed is issued first so the first sample does
/// not carry one-time costs (lazy allocations, thread-pool spin-up).
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
        let query = Query {
            text: text.clone(),
            scope: None,
            k: k.max(1),
            mode: Mode::Hybrid,
            max_distance: None,
            expand: 0,
            rrf_k: DEFAULT_RRF_K,
        };
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

/// The embed projections worth comparing (`embed::projection`, SPEC §6/§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    /// `title\nbody` — the shipped default.
    Full,
    /// `body` only.
    BodyOnly,
    /// `title` only.
    TitleOnly,
}

impl Projection {
    /// Every variant, in report order.
    pub const ALL: [Projection; 3] = [
        Projection::Full,
        Projection::BodyOnly,
        Projection::TitleOnly,
    ];

    /// Canonical name.
    pub fn as_str(self) -> &'static str {
        match self {
            Projection::Full => "title+body",
            Projection::BodyOnly => "body",
            Projection::TitleOnly => "title",
        }
    }

    /// The text this projection hands the embedder for one memory.
    pub fn text(self, title: &str, body: &str) -> String {
        match self {
            Projection::Full => projection::embed_text(title, body),
            Projection::BodyOnly => projection::strip_links(body).trim().to_string(),
            Projection::TitleOnly => title.trim().to_string(),
        }
    }
}

/// One projection's retrieval score over the golden set.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectionScore {
    /// The projection tried.
    pub projection: &'static str,
    /// Its score (KNN-leg only — a projection only moves document vectors).
    pub eval: Eval,
}

/// Compare the embed projections on a fixed corpus without touching the store: embed every
/// document under each variant, rank by cosine distance, and score the golden set (PLAN §6
/// P10: "sweep … projection"). Vectors are L2-normalised, so cosine distance is `1 - dot`.
pub fn sweep_projections(
    embedder: &dyn Embedder,
    corpus: &[Memory],
    cases: &[Case],
    k: usize,
    rrf_k: f64,
) -> Result<Vec<ProjectionScore>> {
    let mut out = Vec::with_capacity(Projection::ALL.len());
    for projection in Projection::ALL {
        let docs: Vec<Vec<f32>> = corpus
            .iter()
            .map(|m| embedder.embed_one(&projection.text(&m.title, &m.body)))
            .collect::<Result<_>>()?;

        let mut hits = 0usize;
        let mut reciprocal_ranks = 0.0f64;
        for case in cases {
            let cutoff = case.k.unwrap_or(k).max(1);
            let query = embedder.embed_query(&case.query)?;
            let mut ranked: Vec<(usize, f32)> = docs
                .iter()
                .enumerate()
                .map(|(i, d)| (i, cosine_distance(&query, d)))
                .collect();
            ranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let rank = ranked
                .iter()
                .take(cutoff)
                .position(|(i, _)| case.expect.contains(&corpus[*i].key));
            if let Some(rank) = rank {
                hits += 1;
                reciprocal_ranks += 1.0 / (rank + 1) as f64;
            }
        }
        let n = cases.len();
        let denom = n.max(1) as f64;
        out.push(ProjectionScore {
            projection: projection.as_str(),
            eval: Eval {
                cases: n,
                hits,
                recall: hits as f64 / denom,
                mrr: reciprocal_ranks / denom,
                k,
                rrf_k,
                mode: "vec".to_string(),
            },
        });
    }
    Ok(out)
}

/// Cosine distance for L2-normalised vectors: `1 - dot`.
fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    1.0 - a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
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
    fn projection_text_matches_the_shipped_default() {
        assert_eq!(Projection::Full.text(" T ", "Body [[x]]"), "T\nBody");
        assert_eq!(Projection::BodyOnly.text(" T ", "Body [[x]]"), "Body");
        assert_eq!(Projection::TitleOnly.text(" T ", "Body [[x]]"), "T");
    }

    #[test]
    fn cosine_distance_is_zero_for_identical_unit_vectors() {
        let v = vec![0.6f32, 0.8];
        assert!(cosine_distance(&v, &v).abs() < 1e-6);
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
                rrf_k: 60.0,
                mode: "hybrid".into(),
            },
            Eval {
                cases: 10,
                hits: 8,
                recall: 0.8,
                mrr: 0.7,
                k: 5,
                rrf_k: 60.0,
                mode: "hybrid".into(),
            },
            Eval {
                cases: 10,
                hits: 7,
                recall: 0.7,
                mrr: 0.9,
                k: 10,
                rrf_k: 20.0,
                mode: "hybrid".into(),
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
            rrf_k: 60.0,
            mode: "hybrid".into(),
        };
        assert!(eval.passes());
        eval.recall = 0.79;
        assert!(!eval.passes());
    }
}
