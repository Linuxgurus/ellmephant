use crate::search::{Bundle, Fields};

pub fn to_wire_json(bundle: &Bundle, fields: &Fields) -> crate::error::Result<String> {
    let hits: Vec<serde_json::Value> = bundle.hits.iter().map(|h| fields.row(h)).collect();
    let header: String = fields
        .names()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(",");

    let mut out = format!("{{\"cols\":[{header}],\"hits\":");
    out.push_str(&crate::wire::rows_array(&hits));

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
