//! The wire form of backlinks and dangling references (soma/ellmephant#236).
//!
//! These lived in `store::links` — a row module that also knew how a row is *presented*. The epic's
//! boundary is the other way round: `store::` knows rows, `wire::` knows the wire.

use crate::error::Result;
use crate::store::links::{Backlink, DanglingEdge, Kind};

/// Positional wire form for both doors: `cols` names the fields once, rows are bare arrays.
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

/// Serialize a [`Backlink`] slice for the MCP wire: a top-level `cols` header names the fields
/// and `backlinks` are positional rows, so field names appear once rather than on every row
/// (mirrors [`crate::search::to_wire_json`]). The keyed serde form stays for scripts. Each row
/// sits on its own line (see [`crate::wire::rows_array`]) so a raw result is scannable.
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

/// How many distinct targets a dangling set names — carried beside the rows because the caller
/// usually wants the count more than the list.
pub fn dangling_targets(edges: &[DanglingEdge]) -> usize {
    let mut targets: Vec<(&str, Kind)> = edges
        .iter()
        .map(|e| (e.to_key.as_str(), e.to_type))
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets.len()
}
