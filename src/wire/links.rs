use crate::error::Result;
use crate::store::links::{Backlink, DanglingEdge, Kind};

pub fn dangling_to_wire_json(edges: &[DanglingEdge]) -> String {
    let rows: Vec<serde_json::Value> = edges
        .iter()
        .map(|e| serde_json::json!([e.from_key, e.to_key, e.to_type.as_str()]))
        .collect();
    format!(
        "{{\"cols\":[\"from\",\"to\",\"to_type\"],\"targets\":{},\"dangling\":{}}}",
        dangling_targets(edges),
        crate::wire::rows_array(&rows)
    )
}

pub fn backlinks_to_wire_json(rows: &[Backlink]) -> Result<String> {
    let backlinks: Vec<serde_json::Value> = rows
        .iter()
        .map(|b| serde_json::json!([b.key, b.title]))
        .collect();
    Ok(format!(
        "{{\"cols\":[\"key\",\"title\"],\"backlinks\":{}}}",
        crate::wire::rows_array(&backlinks)
    ))
}

pub fn dangling_targets(edges: &[DanglingEdge]) -> usize {
    let mut targets: Vec<(&str, Kind)> = edges
        .iter()
        .map(|e| (e.to_key.as_str(), e.to_type))
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets.len()
}
