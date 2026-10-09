//! Shared helpers for the MCP-facing wire (the compact positional tables).
//!
//! The list-shaped reads emit a `cols` header plus bare-array rows. Rendering each row on its
//! own line costs one byte each but turns a single wall-of-blob payload into something a human
//! can scan. JSON treats the whitespace as insignificant, so the result is still valid and
//! machine-parseable — it is layout, not structure.

/// Serialize a positional table body: a JSON array of rows, **one row per line, no indent**.
///
/// Empty yields `[]`; otherwise `[\n<row>,\n<row>\n]`. Each row is compact — a body's newlines
/// arrive JSON-escaped (`\n`), so a row never spans physical lines.
pub fn rows_array(rows: &[serde_json::Value]) -> String {
    if rows.is_empty() {
        return "[]".to_string();
    }
    let mut out = String::from("[\n");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        // A `Value` of scalars/arrays/objects always serializes; `null` is the unreachable guard.
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
        // Whitespace is layout only — it parses back to the same values.
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
