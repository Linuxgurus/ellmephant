//! Integration: the P10 instruments against a real store file (offline).
//!
//! The golden corpus (`tests/golden/`) is imported with the deterministic [`HashEmbedder`],
//! so the whole thing runs with no model and no network. The seed queries are keyword-anchored
//! on purpose: a bag-of-words fake can only measure token overlap, not meaning — real usage
//! replaces the corpus *and* the queries (see `tests/golden/README.md`).

mod common;

use common::temp_store;
use llmphant::bench;
use llmphant::db;
use llmphant::embed::HashEmbedder;
use llmphant::store::{self, Memory};

const DIM: usize = 512;

fn golden(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Read the canonical-JSONL corpus into `Memory` drafts (ids/hashes are derived on insert).
fn read_corpus(path: &std::path::Path) -> Vec<Memory> {
    std::fs::read_to_string(path)
        .expect("read corpus")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("corpus line"))
        .collect()
}

#[test]
fn golden_eval_clears_the_recall_gate() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);

    let corpus = read_corpus(&golden("memories.jsonl"));
    assert!(corpus.len() >= 20, "the corpus should be a real set");
    for memory in &corpus {
        store::add(&conn, &embedder, memory).expect("seed");
    }

    let cases = bench::load_cases(&golden("queries.jsonl")).expect("golden queries");
    assert!(cases.len() >= 20, "PLAN §6 P10 wants >= 20 questions");

    let eval =
        bench::evaluate(&conn, Some(&embedder), &cases, bench::DEFAULT_EVAL_K).expect("evaluate");

    assert_eq!(eval.cases, cases.len());
    assert!(
        eval.passes(),
        "recall@{} = {:.3} below the {:.2} gate ({}/{})",
        eval.k,
        eval.recall,
        bench::RECALL_TARGET,
        eval.hits,
        eval.cases
    );
    assert!(eval.mrr > 0.5, "mrr = {:.3} looks wrong", eval.mrr);
}

#[test]
fn sweep_walks_every_k_and_names_a_winner() {
    let (_dir, paths) = temp_store();
    let conn = db::open(&paths).expect("open");
    let embedder = HashEmbedder::new(DIM);
    for memory in read_corpus(&golden("memories.jsonl")) {
        store::add(&conn, &embedder, &memory).expect("seed");
    }
    let cases = bench::load_cases(&golden("queries.jsonl")).expect("golden queries");

    let evals = bench::sweep(&conn, Some(&embedder), &cases, &bench::SWEEP_KS).expect("sweep");

    assert_eq!(evals.len(), bench::SWEEP_KS.len());
    assert!(evals.iter().all(|e| e.cases == cases.len()));
    assert!(bench::best(&evals).is_some());
}
