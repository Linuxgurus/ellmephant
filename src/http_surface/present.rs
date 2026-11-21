use super::*;

pub trait Present {
    fn json(&self) -> String;
    fn text(&self) -> String;
}

pub fn present<T: Present>(status: StatusCode, json: bool, value: &T) -> ApiResult<Response> {
    render(status, json, value.json(), value.text())
}

pub struct MemoryCard<'a> {
    pub mem: &'a store::Memory,
    pub relation: Option<&'a str>,
    pub version: Option<i64>,
}

impl Present for MemoryCard<'_> {
    fn json(&self) -> String {
        let mut value = serde_json::to_value(self.mem).unwrap_or(serde_json::Value::Null);
        if let Some(obj) = value.as_object_mut() {
            if let Some(n) = self.version {
                obj.insert("version".to_string(), serde_json::json!(n));
            }
            obj.insert("_nudge".to_string(), serde_json::json!(GET_NUDGE));
            if let Some(walk) = self.relation {
                obj.insert("owner_relation".to_string(), serde_json::json!(walk));
            }
        }
        value.to_string()
    }

    fn text(&self) -> String {
        memory_text(self.mem, self.relation)
    }
}

pub struct Table {
    label: String,
    wire: String,
}

impl Table {
    pub fn new(label: impl Into<String>, wire: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            wire: wire.into(),
        }
    }
}

impl Present for Table {
    fn json(&self) -> String {
        self.wire.clone()
    }

    fn text(&self) -> String {
        list_text(&self.label, &self.wire)
    }
}

pub struct Viewed<'a, T> {
    value: &'a T,
    text: fn(&T) -> String,
}

pub fn viewed<'a, T: serde::Serialize>(value: &'a T, text: fn(&T) -> String) -> Viewed<'a, T> {
    Viewed { value, text }
}

impl<T: serde::Serialize> Present for Viewed<'_, T> {
    fn json(&self) -> String {
        serde_json::to_string(self.value).unwrap_or_else(|_| "{}".into())
    }

    fn text(&self) -> String {
        (self.text)(self.value)
    }
}

pub struct Report<T> {
    value: T,
    json: fn(&T) -> String,
    text: fn(&T) -> String,
}

pub fn report<T>(value: T, json: fn(&T) -> String, text: fn(&T) -> String) -> Report<T> {
    Report { value, json, text }
}

impl<T> Present for Report<T> {
    fn json(&self) -> String {
        (self.json)(&self.value)
    }

    fn text(&self) -> String {
        (self.text)(&self.value)
    }
}

pub(super) fn list_text(label: &str, wire: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(wire) else {
        return wire.to_string();
    };
    match first_table(&value) {
        Some(rows) => {
            let mut out = format!("{label} — {} row(s)\n", rows.len());
            for row in rows {
                let cells: Vec<String> = row
                    .as_array()
                    .map(|cells| cells.iter().map(cell).collect())
                    .unwrap_or_default();
                out.push_str("  ");
                out.push_str(&cells.join("  "));
                out.push('\n');
            }
            out
        }
        None => wire.to_string(),
    }
}

pub(super) fn first_table(value: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    let obj = value.as_object()?;
    if obj.contains_key("cols") {
        for (key, v) in obj {
            if key == "cols" {
                continue;
            }
            if let Some(rows) = v.as_array()
                && rows.iter().all(|r| r.is_array())
            {
                return Some(rows.clone());
            }
        }
    }
    for v in obj.values() {
        if let Some(rows) = first_table(v) {
            return Some(rows);
        }
    }
    None
}

pub(super) fn cell(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items.iter().map(cell).collect::<Vec<_>>().join(","),
        serde_json::Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

pub(super) fn stats_text(s: &store::stats::Stats) -> String {
    let mut out = format!(
        "ellmephant store — {} memories, {} bytes on disk\n",
        s.memories, s.db_bytes
    );
    for (label, value) in [
        ("body chars", s.body_chars.to_string()),
        ("distinct tags", s.distinct_tags.to_string()),
        ("tagged", format!("{}/{}", s.tagged, s.memories)),
        ("link edges", s.edges.to_string()),
        ("orphans", s.orphans.to_string()),
        ("dangling", s.dangling.to_string()),
        ("commons", s.commons.to_string()),
        (
            "unattributed",
            match s.unattributed {
                0 => "0".to_string(),
                n => format!(
                    "{n} (no currently existing pen wrote these — no owner, no author; claim them)"
                ),
            },
        ),
        ("visits", s.visits_total.to_string()),
        ("useful", s.useful_total.to_string()),
        (
            "versions",
            format!(
                "{} row(s), {} bytes, {} of a deleted key",
                s.versions, s.version_bytes, s.version_orphans
            ),
        ),
        (
            "encode queue",
            match s.encode_oldest_s {
                Some(age) => format!(
                    "{} pending (oldest {}s), {} failed",
                    s.encode_pending, age, s.encode_failed
                ),
                None => format!("{} pending, {} failed", s.encode_pending, s.encode_failed),
            },
        ),
    ] {
        out.push_str(&format!("  {label:<14}{value}\n"));
    }
    for (tag, count) in &s.top_tags {
        out.push_str(&format!("  top tag       {tag} ({count})\n"));
    }
    for (key, count) in &s.top_useful {
        out.push_str(&format!("  useful        {key} ({count})\n"));
    }
    out
}

pub(super) fn health_text(h: &store::health::Health) -> String {
    let pct = |v: Option<f64>| match v {
        Some(n) => format!("{n:.2}%"),
        None => "null".to_string(),
    };
    let mut out = format!("ellmephant memory health — scope: {}\n", h.scope);
    for (label, value) in [
        ("rows", h.rows.to_string()),
        ("body_chars", h.body_chars.to_string()),
        ("log_chars", h.log_chars.to_string()),
        ("log_share", pct(h.log_share)),
        (
            "dup_pairs",
            match h.dup_pairs {
                Some(n) => n.to_string(),
                None => "null".to_string(),
            },
        ),
        (
            "dup_min_distance",
            match h.dup_pairs_min_distance {
                Some(d) => format!("{d:.4}"),
                None => "null".to_string(),
            },
        ),
        ("woven", pct(h.woven)),
        ("woven_inbound", pct(h.woven_inbound)),
        ("woven_outbound", pct(h.woven_outbound)),
        ("cold30", pct(h.cold30)),
        ("cold30_population", h.cold30_population.to_string()),
        ("tag_distinct", h.tag_distinct.to_string()),
        ("tag_singletons", h.tag_singletons.to_string()),
        ("rows_untagged", h.rows_untagged.to_string()),
        ("edges", h.edges.to_string()),
        (
            "edges_per_row",
            match h.edges_per_row {
                Some(v) => format!("{v:.4}"),
                None => "null".to_string(),
            },
        ),
        ("dangling", h.dangling.to_string()),
    ] {
        out.push_str(&format!("  {label:<18}{value}\n"));
    }
    for reason in &h.suppressed {
        out.push_str(&format!("  suppressed        {reason}\n"));
    }
    out.push('\n');
    out.push_str(h.scope_note);
    out.push('\n');
    out
}

pub(super) fn memory_text(mem: &store::Memory, relation: Option<&str>) -> String {
    let mut out = format!("{}\n", mem.key);
    out.push_str(&format!("  {:<8}{}\n", "title", mem.title));
    if let Some(owner) = &mem.owner {
        match relation {
            Some(walk) => out.push_str(&format!("  {:<8}{owner}  {walk}\n", "owner")),
            None => out.push_str(&format!("  {:<8}{owner}\n", "owner")),
        }
    }
    out.push_str(&format!("  {:<8}{}\n", "tags", mem.tags.join(", ")));
    if let Some(source) = &mem.source {
        out.push_str(&format!("  {:<8}{source}\n", "source"));
    }
    out.push_str(&format!(
        "  {:<8}{} visits, {} useful\n",
        "usage", mem.visits, mem.times_useful
    ));
    out.push_str("---\n");
    out.push_str(&mem.body);
    out.push('\n');
    out
}
