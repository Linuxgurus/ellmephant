//! The wire form of a project list and a project view (soma/ellmephant#236). `store::project`
//! gained rows and lost the knowledge of how they are rendered.

use crate::error::Result;
use crate::store::project::{Project, View};

/// `project_list` for the MCP wire: a `cols` header plus positional rows, one per line (the
/// convention for list-shaped reads; `state` is deliberately absent — fetch it with `project_get`).
/// The dates *are* here: "what did I finish last month" is a list question.
pub fn list_to_wire_json(rows: &[Project]) -> Result<String> {
    let rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|p| {
            serde_json::json!([
                p.key,
                p.status,
                p.owner,
                p.tags,
                p.title,
                p.goal,
                p.start_date,
                p.end_date
            ])
        })
        .collect();
    Ok(format!(
        "{{\"cols\":[\"key\",\"status\",\"owner\",\"tags\",\"title\",\"goal\",\"start_date\",\
          \"end_date\"],\"projects\":{}}}",
        crate::wire::rows_array(&rows)
    ))
}

/// `project_get` for the MCP wire: the project as a keyed object (one row, many fields) plus the
/// ordered preload as its own `cols`/`rows` table.
pub fn view_to_wire_json(view: &View) -> Result<String> {
    let preload: Vec<serde_json::Value> = view
        .preload
        .iter()
        .map(|r| serde_json::json!([r.key, r.title]))
        .collect();
    let mut out = format!(
        "{{\"project\":{},\"preload\":{{\"cols\":[\"key\",\"title\"],\"rows\":{}}}",
        serde_json::to_string(&view.project)?,
        crate::wire::rows_array(&preload),
    );
    // Hydrated bodies ride in a SISTER table rather than widening `preload`'s columns: the lean
    // default shape stays byte-identical (the table appears only when something was hydrated), a
    // caller that asked for bodies finds them under one key, and the shared `[key, title]` table is
    // never padded with nulls. Rows keep the preload order; a key that resolves to nothing has no
    // body and so is absent here (it is still *shown* in `preload`).
    let bodies: Vec<serde_json::Value> = view
        .preload
        .iter()
        .filter_map(|r| r.body.as_ref().map(|body| serde_json::json!([r.key, body])))
        .collect();
    if !bodies.is_empty() {
        out.push_str(&format!(
            ",\"bodies\":{{\"cols\":[\"key\",\"body\"],\"rows\":{}}}",
            crate::wire::rows_array(&bodies)
        ));
    }
    out.push('}');
    Ok(out)
}
