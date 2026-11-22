use ellmephant::embed::qwen3::Options;
use ellmephant::embed::{Embedder, HashEmbedder};

#[test]
fn options_default_to_the_documented_model_and_length() {
    let opts = Options::new(1024, "cpu", "bf16", "");
    assert_eq!(opts.model_id, ellmephant::embed::qwen3::MODEL_ID);
    assert_eq!(opts.dim, 1024);
    assert_eq!(opts.device, "cpu");
    assert_eq!(opts.precision, "bf16");
    assert_eq!(opts.max_len, 8192);
}

#[test]
fn symmetric_embedder_reuses_embed_one_for_queries() {
    let e = HashEmbedder::new(32);
    assert_eq!(
        e.embed_query("where are the logs").unwrap(),
        e.embed_one("where are the logs").unwrap()
    );
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
#[ignore = "needs downloaded Qwen3 weights (ELLMEPHANT_TEST_MODEL_DIR)"]
fn qwen3_is_deterministic_normalised_and_self_similar() {
    let dir = std::env::var("ELLMEPHANT_TEST_MODEL_DIR")
        .expect("set ELLMEPHANT_TEST_MODEL_DIR to a flat weights dir");
    let text =
        std::env::var("ELLMEPHANT_TEST_TEXT").unwrap_or_else(|_| "the quick brown fox".into());

    let opts = Options::new(1024, "cpu", "bf16", "");
    let embedder = ellmephant::embed::qwen3::Qwen3Embedder::load(std::path::Path::new(&dir), &opts)
        .expect("load encoder");

    let a = embedder.embed_one(&text).expect("embed a");
    let b = embedder.embed_one(&text).expect("embed b");
    assert_eq!(a.len(), 1024, "Qwen3-Embedding-0.6B is 1024-d");
    assert_eq!(a, b, "same text must give the same vector");

    let norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "expected unit norm, got {norm}");
    assert!(
        (cosine(&a, &b) - 1.0).abs() < 1e-4,
        "self-cosine should be ~1"
    );

    let c = embedder
        .embed_one("a completely unrelated sentence")
        .unwrap();
    assert!(
        cosine(&a, &c) < 0.999,
        "unrelated text should not be identical"
    );
}

/// ★ THE POINT OF BATCHING, STATED AS A TEST. `embed` now stacks rows into one padded
/// `[B,T]` forward and pools each row at its OWN last real token. Right padding plus a
/// causal mask makes that the same computation as one row at a time — this asserts it
/// against real weights rather than trusting the argument in `forward_batch`.
#[test]
#[ignore = "needs downloaded Qwen3 weights (ELLMEPHANT_TEST_MODEL_DIR)"]
fn a_batch_matches_one_document_at_a_time() {
    let dir = std::env::var("ELLMEPHANT_TEST_MODEL_DIR")
        .expect("set ELLMEPHANT_TEST_MODEL_DIR to a flat weights dir");
    let opts = Options::new(1024, "cpu", "bf16", "");
    let embedder = ellmephant::embed::qwen3::Qwen3Embedder::load(std::path::Path::new(&dir), &opts)
        .expect("load encoder");

    // Deliberately ragged: a long one, a short one, and a one-word one, so the padded
    // tensor has real work to mis-handle if the pooling index or the padding side is wrong.
    let texts = vec![
        "Memory is the only layer this process edits; everything else is reloaded.".to_string(),
        "short one".to_string(),
        "hello".to_string(),
    ];

    let one_by_one: Vec<Vec<f32>> = texts
        .iter()
        .map(|t| embedder.embed_one(t).expect("single"))
        .collect();
    let batched = embedder.embed(&texts).expect("batch");
    assert_eq!(batched.len(), texts.len());

    for (i, (single, batch)) in one_by_one.iter().zip(batched.iter()).enumerate() {
        let worst = single
            .iter()
            .zip(batch)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst < 1e-4,
            "row {i}: batched differs from single by {worst} — padding, pooling index or \
             mask semantics are wrong"
        );
    }
}
