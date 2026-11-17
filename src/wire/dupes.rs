//! The wire form of a merge plan (soma/ellmephant#236): the steps, and the pairs refused with
//! their reason. Moved out of `store::dupes` for the same reason as its siblings.

use crate::error::Result;
use crate::store::dupes::{DupePair, MergePlan};

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
