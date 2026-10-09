//! Output formatting: bordered tables for humans, `--json` for scripts.
//!
//! Tables use `tabled` from P6 onward; until then this renders one hit per block so
//! `--json` and the human path stay interchangeable.

use crate::error::Result;
use crate::search::{Bundle, Hit};

/// Serialise any hit list as a JSON array.
pub fn hits_to_json(hits: &[Hit]) -> Result<String> {
    Ok(serde_json::to_string_pretty(hits)?)
}

/// Render a search bundle for a terminal (or `--json`), ranked first.
pub fn render_bundle(bundle: &Bundle, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(bundle)?);
    }
    let mut out = render_hits(&bundle.hits, false)?;
    if !bundle.neighbors.is_empty() {
        out.push_str(&format!("\n{} linked:\n", bundle.neighbors.len()));
        for n in &bundle.neighbors {
            out.push_str(&format!(
                "  hop {}  [[{}]] -> {}  {}\n",
                n.hop, n.via, n.memory.key, n.memory.title
            ));
        }
    }
    Ok(out)
}

/// Render hits for a terminal, newest-ranked first.
pub fn render_hits(hits: &[Hit], json: bool) -> Result<String> {
    if json {
        return hits_to_json(hits);
    }
    if hits.is_empty() {
        return Ok("(no matches)".to_string());
    }
    let mut out = String::new();
    for (rank, hit) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {:.4}  {}  [{}]\n   {}\n",
            rank + 1,
            hit.score,
            hit.key,
            hit.scope,
            hit.title
        ));
        if !hit.tags.is_empty() {
            out.push_str(&format!("   tags: {}\n", hit.tags.join(", ")));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit() -> Hit {
        Hit {
            id: 1,
            key: "infra-gitops".into(),
            scope: "infra".into(),
            type_: "fact".into(),
            title: "GitOps & registry".into(),
            body: "argocd".into(),
            score: 0.5,
            distance: Some(0.25),
            tags: vec!["k8s".into()],
        }
    }

    #[test]
    fn json_roundtrips() {
        let json = hits_to_json(&[hit()]).unwrap();
        assert!(json.contains("\"infra-gitops\""));
        assert!(json.trim_start().starts_with('['));
    }

    #[test]
    fn empty_is_explicit() {
        assert_eq!(render_hits(&[], false).unwrap(), "(no matches)");
        assert_eq!(render_hits(&[], true).unwrap(), "[]");
    }
}
