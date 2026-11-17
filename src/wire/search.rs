//! The search wire: a `cols` header and positional rows, the shape both doors hand a caller.
//!
//! Split out of `search/mod.rs` (soma/ellmephant#236): the search layer ANSWERS a question; how
//! that answer is laid out for a transport is `wire`'s business. Nothing here reads the store.

use crate::search::{Bundle, Fields};

/// Serialize a [`Bundle`] for the wire: a top-level `cols` header names the hit fields and
/// `hits` are positional rows, so the payload stays self-describing without repeating field
/// names on every row. `fields` decides which columns ride along (see [`Fields`]; `key` is always
/// present). Link `neighbors`, when present, carry their own `cols`/`rows` table.
///
/// This is the compact LLM-facing shape (fewer tokens than keyed JSON); the keyed serde form
/// remains the stable machine face used by `--json` and `structuredContent`. Each row sits on
/// its own line (see [`crate::wire::rows_array`]) so a raw result is scannable, not one blob.
pub fn to_wire_json(bundle: &Bundle, fields: &Fields) -> crate::error::Result<String> {
    let hits: Vec<serde_json::Value> = bundle.hits.iter().map(|h| fields.row(h)).collect();
    let header: String = fields
        .names()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(",");

    let mut out = format!("{{\"cols\":[{header}],\"hits\":");
    out.push_str(&crate::wire::rows_array(&hits));

    // ★ THE TWO LEGS, EACH IN ITS OWN ORDER (2026-11-11). `hits` stays for one release as the fused
    // view; these are the answer a caller can actually trust — a row's position in `semantic` is its
    // distance, and its position in `lexical` is its BM25. Absent when that leg did not run, so an
    // empty array never impersonates "the leg found nothing".
    if let Some(hits) = &bundle.semantic {
        let rows: Vec<serde_json::Value> = hits.iter().map(|h| fields.row(h)).collect();
        out.push_str(",\"semantic\":");
        out.push_str(&crate::wire::rows_array(&rows));
    }
    if let Some(hits) = &bundle.lexical {
        let rows: Vec<serde_json::Value> = hits.iter().map(|h| fields.row(h)).collect();
        out.push_str(",\"lexical\":");
        out.push_str(&crate::wire::rows_array(&rows));
    }

    // The instrumentation rides the envelope so it cannot be missed by a caller who never learned
    // to look at a hit's `via`: `semantic_candidates: 0` says the phrase leg answered nothing.
    if let Some(m) = &bundle.search {
        let ceiling = match m.max_distance {
            Some(d) => format!("{}", crate::wire::wire_distance(d)),
            None => "null".to_string(),
        };
        let preds: String = m
            .predicates
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&format!(
            ",\"search\":{{\"max_distance\":{ceiling},\"semantic_candidates\":{},\"lexical_candidates\":{},\"predicates\":[{preds}]}}",
            m.semantic_candidates, m.lexical_candidates
        ));
    }

    if !bundle.neighbors.is_empty() {
        let rows: Vec<serde_json::Value> = bundle
            .neighbors
            .iter()
            .map(|n| {
                serde_json::json!([
                    n.hop,
                    n.via,
                    n.memory.key,
                    n.memory.title,
                    n.memory.tags,
                    n.memory.body
                ])
            })
            .collect();
        out.push_str(
            ",\"neighbors\":{\"cols\":[\"hop\",\"via\",\"key\",\"title\",\"tags\",\"body\"],\"rows\":",
        );
        out.push_str(&crate::wire::rows_array(&rows));
        out.push('}');
    }
    out.push('}');
    Ok(out)
}
