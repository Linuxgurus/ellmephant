use crate::error::Result;
use crate::store::project::{Project, View};

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
