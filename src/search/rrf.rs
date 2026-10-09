//! Reciprocal Rank Fusion: merges the BM25 and KNN ranked lists by rank, not by
//! score, which sidesteps the fact that the two scores are not comparable.

use std::collections::BTreeMap;

/// Standard RRF constant (SPEC §10).
pub const DEFAULT_RRF_K: f64 = 60.0;

/// Fuse rankings of `memory.id` into `(id, score)` sorted best-first.
///
/// `score = Σ 1 / (k + rank)` with 1-based `rank`; ties break on ascending id so the
/// output is stable run to run.
pub fn fuse(rankings: &[Vec<i64>], k: f64) -> Vec<(i64, f64)> {
    let mut scores: BTreeMap<i64, f64> = BTreeMap::new();
    for ranking in rankings {
        for (i, id) in ranking.iter().enumerate() {
            let rank = (i + 1) as f64;
            *scores.entry(*id).or_insert(0.0) += 1.0 / (k + rank);
        }
    }
    let mut fused: Vec<(i64, f64)> = scores.into_iter().collect();
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agrees_on_top_hit() {
        let fused = fuse(&[vec![7, 8], vec![7, 9]], DEFAULT_RRF_K);
        assert_eq!(fused[0].0, 7);
    }

    #[test]
    fn single_list_preserves_order() {
        let ids: Vec<i64> = fuse(&[vec![3, 1, 2]], DEFAULT_RRF_K)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![3, 1, 2]);
    }

    #[test]
    fn ties_break_on_id() {
        let fused = fuse(&[vec![5], vec![2]], DEFAULT_RRF_K);
        assert_eq!(
            fused.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![2, 5]
        );
        assert!((fused[0].1 - fused[1].1).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(fuse(&[], DEFAULT_RRF_K).is_empty());
        assert!(fuse(&[vec![]], DEFAULT_RRF_K).is_empty());
    }
}
