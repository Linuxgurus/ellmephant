pub mod dupes;
pub mod links;
pub mod project;
pub mod search;

pub fn wire_distance(d: f32) -> f64 {
    ((d as f64) * 10_000.0).round() / 10_000.0
}

pub fn rows_array(rows: &[serde_json::Value]) -> String {
    if rows.is_empty() {
        return "[]".to_string();
    }
    let mut out = String::from("[\n");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str(&serde_json::to_string(row).unwrap_or_else(|_| "null".to_string()));
    }
    out.push_str("\n]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_a_compact_pair() {
        assert_eq!(rows_array(&[]), "[]");
    }

    #[test]
    fn one_row_per_line_no_indent_and_still_valid_json() {
        let rows = vec![
            serde_json::json!(["a", "A", ["t1"]]),
            serde_json::json!(["b", "B", []]),
        ];
        let out = rows_array(&rows);
        assert_eq!(out, "[\n[\"a\",\"A\",[\"t1\"]],\n[\"b\",\"B\",[]]\n]");
        assert_eq!(out.lines().count(), 4, "open, two rows, close: {out}");
        let back: Vec<serde_json::Value> = serde_json::from_str(&out).expect("valid json");
        assert_eq!(back, rows);
    }

    #[test]
    fn an_embedded_newline_is_escaped_not_a_break() {
        let rows = vec![serde_json::json!(["k", "line1\nline2"])];
        let out = rows_array(&rows);
        assert_eq!(out.lines().count(), 3, "row must stay on one line: {out}");
        let back: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(back[0][1], serde_json::json!("line1\nline2"));
    }
}
