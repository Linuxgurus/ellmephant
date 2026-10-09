//! `llmphant bench` — real latency numbers and the golden eval (PLAN §6 P10).
//!
//! `bench` loads the encoder (that *is* the measured cold start), times embed and hybrid
//! search, and — with `--golden` — scores a JSONL query set. `--sweep` additionally walks the
//! query-time knobs (result count, RRF weight) and compares the embed projections.

use std::path::PathBuf;
use std::time::Instant;

use crate::bench::{self, Case};
use crate::config::{Config, Paths};
use crate::db;
use crate::error::Result;
use crate::model;
use crate::search::{DEFAULT_RRF_K, Mode};
use crate::store;

use super::embed::load_embedder;

/// `bench` flags (the clap [`Command::Bench`](super::Command::Bench) fields).
#[derive(Debug, Clone)]
pub struct Options {
    /// Also score the golden query set.
    pub golden: bool,
    /// Golden JSONL path (default: `<home>/golden/queries.jsonl`).
    pub golden_file: Option<PathBuf>,
    /// Sweep k, the RRF weight and the embed projection.
    pub sweep: bool,
    /// Result count for the timing search and the single-shot golden run.
    pub k: usize,
}

/// Run the benchmark against the store at `paths`.
pub fn run(paths: &Paths, cfg: &Config, spec: &model::Spec, opts: &Options) -> Result<()> {
    let conn = db::open(paths)?;

    // Loading the encoder is the cold-start cost SPEC §13 calls the real one.
    let load_started = Instant::now();
    let embedder = load_embedder(spec, cfg)?;
    let load_ms = load_started.elapsed().as_secs_f64() * 1e3;

    let golden_path = opts
        .golden_file
        .clone()
        .unwrap_or_else(|| default_golden(paths));
    let cases: Vec<Case> = if opts.golden || opts.sweep {
        bench::load_cases(&golden_path)?
    } else {
        Vec::new()
    };

    // The golden questions double as embed samples: that is the text distribution that matters.
    let queries: Vec<String> = if cases.is_empty() {
        default_queries()
    } else {
        cases.iter().map(|c| c.query.clone()).collect()
    };

    let timings = bench::timings(&conn, &embedder, load_ms, &queries, &queries, opts.k)?;
    print_timings(&timings);

    if opts.golden || opts.sweep {
        let eval = bench::evaluate(
            &conn,
            Some(&embedder),
            &cases,
            opts.k,
            DEFAULT_RRF_K,
            Mode::Hybrid,
        )?;
        print_eval("hybrid", &eval);
    }

    if opts.sweep {
        let evals = bench::sweep(
            &conn,
            Some(&embedder),
            &cases,
            &bench::SWEEP_KS,
            &bench::SWEEP_RRF_KS,
            Mode::Hybrid,
        )?;
        print_sweep(&evals);

        let corpus = store::all(&conn)?;
        let scores = bench::sweep_projections(&embedder, &corpus, &cases, opts.k, DEFAULT_RRF_K)?;
        print_projections(&scores);
    }
    Ok(())
}

/// The default golden set lives beside the store so an exported memory can carry one.
fn default_golden(paths: &Paths) -> PathBuf {
    paths.root().join("golden").join("queries.jsonl")
}

/// Fallback probes for a store with no golden set: enough to report an embed/search number.
fn default_queries() -> Vec<String> {
    vec![
        "how do we deploy".to_string(),
        "where is the database".to_string(),
        "which host runs the gateway".to_string(),
        "what is the coding style".to_string(),
    ]
}

fn print_timings(t: &bench::Timings) {
    println!("timings");
    println!("  rows          {}", t.rows);
    println!("  dim           {}", t.dim);
    println!("  load          {:.0} ms", t.load_ms);
    println!(
        "  embed         {:.1} ms mean / {:.1} ms p95 ({} sample(s))",
        t.embed.mean_ms, t.embed.p95_ms, t.embed.n
    );
    println!(
        "  search        {:.1} ms mean / {:.1} ms p95 ({} quer(ies))",
        t.search.mean_ms, t.search.p95_ms, t.search.n
    );
}

fn print_eval(label: &str, e: &bench::Eval) {
    println!("golden ({label}, k={})", e.k);
    println!("  cases         {}", e.cases);
    println!(
        "  recall@{}     {:.3}  ({}/{})",
        e.k, e.recall, e.hits, e.cases
    );
    println!("  mrr           {:.3}", e.mrr);
    println!("  passes        {}", if e.passes() { "yes" } else { "no" });
}

fn print_sweep(evals: &[bench::Eval]) {
    println!("sweep (k x rrf, hybrid)");
    for e in evals {
        println!(
            "  k={:<3} rrf={:<5.0} recall={:.3} mrr={:.3}",
            e.k, e.rrf_k, e.recall, e.mrr
        );
    }
    if let Some(best) = bench::best(evals) {
        println!(
            "  best          k={} rrf={:.0} recall={:.3} mrr={:.3}",
            best.k, best.rrf_k, best.recall, best.mrr
        );
    }
}

fn print_projections(scores: &[bench::ProjectionScore]) {
    println!("sweep (projection, knn-only)");
    for s in scores {
        println!(
            "  {:<11} recall@{} {:.3} mrr={:.3}",
            s.projection, s.eval.k, s.eval.recall, s.eval.mrr
        );
    }
}
