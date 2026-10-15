//! The full plain-HTTP surface (HTTP.md slice 2): every MCP tool as a route over the same
//! `Session`, plus `/bootstrap`.
//!
//! Three laws hold here, in order of strength. **Reads are pure** — no `GET` writes, so `GET
//! /memory/{key}` does not bump `visits` (`POST /memory/{key}/visit` does). **`/health` and
//! `/bootstrap` never touch `Session`** — they answer from their own read-only connection, so a
//! read never queues behind a writer. **Auth before writes** — every `POST`/`PUT`/`DELETE` resolves
//! a bearer to a scope first, and a rejection carries `WWW-Authenticate: Bearer` and nothing else.

use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use rusqlite::{Connection, OpenFlags};

use crate::http::{HttpState, probe};
use crate::mcp::{
    CoherentFilter, DUPES_LIMIT, FieldParam, GET_NUDGE, MatchModeParam, MemoryServer, SearchParams,
};
use crate::search::MatchMode;
use crate::store::{self, links::backlinks_to_wire_json, project};

/// One MCP tool reachable over HTTP — the parity table. A route the table does not name is a tool
/// a wedged MCP session would strand; the parity test keeps it exactly as long as `TOOLS`.
pub struct Route {
    /// HTTP method.
    pub method: &'static str,
    /// Path template (`{key}` is the axum 0.8 placeholder).
    pub path: &'static str,
    /// The MCP tool this route mirrors.
    pub tool: &'static str,
}

/// Every MCP tool, and the route that reaches the same `Session` call (`/health` and `/bootstrap`
/// are surfaces of their own, not tools, so they are absent by design).
pub const ROUTES: &[Route] = &[
    Route {
        method: "POST",
        path: "/memory",
        tool: "memory_upsert",
    },
    Route {
        method: "DELETE",
        path: "/memory/{key}",
        tool: "memory_delete",
    },
    Route {
        method: "POST",
        path: "/memory/{key}/merge",
        tool: "memory_merge",
    },
    Route {
        method: "GET",
        path: "/search",
        tool: "memory_search",
    },
    Route {
        method: "GET",
        path: "/memory/{key}",
        tool: "memory_get",
    },
    Route {
        method: "GET",
        path: "/memory/{key}/backlinks",
        tool: "memory_backlinks",
    },
    Route {
        method: "PUT",
        path: "/memory/{key}/coherent",
        tool: "memory_coherent",
    },
    Route {
        method: "POST",
        path: "/memory/{key}/upvote",
        tool: "memory_upvote",
    },
    Route {
        method: "GET",
        path: "/stats",
        tool: "memory_stats",
    },
    Route {
        method: "GET",
        path: "/dupes",
        tool: "memory_dupes",
    },
    Route {
        method: "POST",
        path: "/reembed",
        tool: "memory_reembed",
    },
    Route {
        method: "POST",
        path: "/project",
        tool: "project_upsert",
    },
    Route {
        method: "GET",
        path: "/project/{key}",
        tool: "project_get",
    },
    Route {
        method: "GET",
        path: "/projects",
        tool: "project_list",
    },
];

/// A text or JSON body with an explicit content type.
fn render(status: StatusCode, json: bool, json_body: String, text_body: String) -> Response {
    let (content_type, body) = if json {
        ("application/json", json_body)
    } else {
        ("text/plain; charset=utf-8", text_body)
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("http response")
}

/// An error body: one line of prose, or `{"error","status"}` under `?format=json`.
fn error(json: bool, status: StatusCode, message: impl AsRef<str>) -> Response {
    let message = message.as_ref();
    if json {
        let body = serde_json::json!({ "error": message, "status": status.as_u16() }).to_string();
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .expect("http error")
    } else {
        render(status, false, String::new(), message.to_string())
    }
}

/// 404 for a store error that names a missing row; 400 otherwise (a malformed request).
fn error_status(message: &str) -> StatusCode {
    if message.starts_with("no such memory:") || message.starts_with("no such project:") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    }
}

/// The session shared with `/mcp`, or the 500 that means the router forgot to attach it.
#[allow(clippy::result_large_err)] // the Err is a ready-to-return response
fn session(state: &HttpState) -> Result<&MemoryServer, Response> {
    state.server().ok_or_else(|| {
        error(
            false,
            StatusCode::INTERNAL_SERVER_ERROR,
            "server not attached",
        )
    })
}

/// The decoded query string, as ordered pairs so a repeated key (`tag=a&tag=b`) survives — which
/// `HashMap` extraction would collapse to the last. Small by hand: no percent-decoder dependency.
struct Params(Vec<(String, String)>);

impl Params {
    /// Parse `a=1&b=2`; a bare `?` or `None` yields no pairs.
    fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self(Vec::new());
        };
        Self(
            raw.split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| match pair.split_once('=') {
                    Some((k, v)) => (decode(k), decode(v)),
                    None => (decode(pair), String::new()),
                })
                .collect(),
        )
    }

    /// The first value for `name`.
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Every value for `name`, in order.
    fn all(&self, name: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// A trimmed non-empty value for `name` (`""` reads as absent).
    fn maybe(&self, name: &str) -> Option<&str> {
        self.get(name).map(str::trim).filter(|v| !v.is_empty())
    }

    /// `true` only for `?format=json`.
    fn json(&self) -> bool {
        self.get("format") == Some("json")
    }

    /// Parse `name` as `T`, erroring with a line naming it on failure.
    fn parse_as<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, String> {
        match self.maybe(name) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<T>()
                .map(Some)
                .map_err(|_| format!("bad `{name}`: {raw}")),
        }
    }

    /// A boolean flag (`true`/`false`, default `false`).
    fn flag(&self, name: &str) -> bool {
        matches!(self.maybe(name), Some("true"))
    }
}

/// Percent-decode `+` and `%XX`; a stray `%` is left literal.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One hex digit.
fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

pub async fn stats(State(state): State<HttpState>, RawQuery(raw): RawQuery) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let with_tags = params.flag("tags");
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("memory_stats", move |s| s.stats(with_tags))
        .await
    {
        Ok(stats) => render(
            StatusCode::OK,
            json,
            serde_json::to_string(&stats).unwrap_or_default(),
            stats_text(&stats),
        ),
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /memory/{key}` — one memory whole. **Pure**: it does not bump `visits` (HTTP.md invariant 1);
/// counting lives at `POST /memory/{key}/visit`.
pub async fn memory_get(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server.with_store("memory_get", move |s| s.show(&key)).await {
        Ok(mem) => {
            let mut value = serde_json::to_value(&mem).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = value.as_object_mut() {
                obj.insert("_nudge".to_string(), serde_json::json!(GET_NUDGE));
            }
            render(StatusCode::OK, json, value.to_string(), memory_text(&mem))
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /memory/{key}/backlinks` — incoming `[[…]]` edges (`memory_backlinks`).
pub async fn backlinks(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("memory_backlinks", move |s| s.backlinks(&key))
        .await
    {
        Ok(rows) => {
            let wire = backlinks_to_wire_json(&rows).unwrap_or_default();
            render(
                StatusCode::OK,
                json,
                wire.clone(),
                list_text("backlinks", &wire),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /search` — the same query surface as `memory_search`, same bytes.
pub async fn search(State(state): State<HttpState>, RawQuery(raw): RawQuery) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let search = match search_params(&params) {
        Ok(p) => p,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server.run_search(search).await {
        Ok(wire) => render(StatusCode::OK, json, wire.clone(), list_text("hits", &wire)),
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /dupes` — near-duplicate candidate pairs (`memory_dupes`).
pub async fn dupes(State(state): State<HttpState>, RawQuery(raw): RawQuery) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let max_distance = match params.parse_as::<f32>("max_distance") {
        Ok(v) => v.unwrap_or(store::dupes::DEFAULT_MAX_DISTANCE),
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let limit = match params.parse_as::<usize>("limit") {
        Ok(v) => v.unwrap_or(DUPES_LIMIT),
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("memory_dupes", move |s| s.dupes(max_distance, limit))
        .await
    {
        Ok(pairs) => {
            let wire = store::dupes::to_wire_json(&pairs).unwrap_or_default();
            render(
                StatusCode::OK,
                json,
                wire.clone(),
                list_text("dupes", &wire),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /projects` — the project index (`project_list`), same bytes.
pub async fn projects(State(state): State<HttpState>, RawQuery(raw): RawQuery) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let mode = match mode_param(params.maybe("mode")) {
        Ok(m) => m,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let filter = project::Filter {
        status: params.maybe("status").map(str::to_string),
        tags: params.all("tag"),
        owner: params.all("owner"),
        pattern: params.maybe("pattern").map(str::to_string),
        mode,
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("project_list", move |s| s.project_list(&filter))
        .await
    {
        Ok(rows) => {
            let wire = project::list_to_wire_json(&rows).unwrap_or_default();
            render(
                StatusCode::OK,
                json,
                wire.clone(),
                list_text("projects", &wire),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /project/{key}` — one project with its ordered preload (`project_get`).
pub async fn project_get(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let hydrate = match project::Hydrate::parse(params.get("hydrate").unwrap_or("none")) {
        Ok(h) => h,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e.to_string()),
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("project_get", move |s| s.project_view(&key, hydrate))
        .await
    {
        Ok(view) => {
            let wire = project::view_to_wire_json(&view).unwrap_or_default();
            render(
                StatusCode::OK,
                json,
                wire.clone(),
                list_text("project", &wire),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// The router fallback: our own 404, so a gateway never substitutes an empty body.
pub async fn not_found(uri: axum::http::Uri) -> Response {
    error(
        false,
        StatusCode::NOT_FOUND,
        format!("not found: {}", uri.path()),
    )
}

/// The 405 for a known path, wrong method — again our own body, not the gateway's.
pub async fn method_not_allowed() -> Response {
    error(false, StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

/// Build a `memory_search` request from the query string, so `GET /search` runs the MCP path.
fn search_params(p: &Params) -> Result<SearchParams, String> {
    let mut fields = Vec::new();
    if let Some(raw) = p.maybe("fields") {
        for name in raw.split(',') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            match field_param(name) {
                Some(f) => fields.push(f),
                None => return Err(format!("bad `fields`: {name}")),
            }
        }
    }
    let coherent = match p.maybe("coherent") {
        Some(c) => Some(coherent_param(c)?),
        None => None,
    };
    let mut tags = p.all("tag");
    tags.extend(p.all("tags"));
    Ok(SearchParams {
        query: p.maybe("q").map(str::to_string),
        pattern: p.maybe("pattern").map(str::to_string),
        mode: Some(match_mode_param(p.maybe("mode"))?),
        k: p.parse_as::<usize>("k")?,
        max_distance: p.parse_as::<f32>("max_distance")?,
        expand: p.parse_as::<u8>("expand")?,
        tags,
        min_chars: p.parse_as::<usize>("min_chars")?,
        max_chars: p.parse_as::<usize>("max_chars")?,
        coherent,
        created_after: p.maybe("created_after").map(str::to_string),
        created_before: p.maybe("created_before").map(str::to_string),
        updated_after: p.maybe("updated_after").map(str::to_string),
        updated_before: p.maybe("updated_before").map(str::to_string),
        fields,
    })
}

/// The `mode` as the search layer spells it.
fn match_mode_param(raw: Option<&str>) -> Result<MatchModeParam, String> {
    match raw.map(str::trim).unwrap_or("") {
        "" | "literal" => Ok(MatchModeParam::Literal),
        "regex" => Ok(MatchModeParam::Regex),
        other => Err(format!("bad `mode`: {other}")),
    }
}

/// The `mode` as `project_list` spells it.
fn mode_param(raw: Option<&str>) -> Result<MatchMode, String> {
    match raw.map(str::trim).unwrap_or("") {
        "" | "literal" => Ok(MatchMode::Literal),
        "regex" => Ok(MatchMode::Regex),
        other => Err(format!("bad `mode`: {other}")),
    }
}

/// One hit field name, or `None` if it is not a field.
fn field_param(name: &str) -> Option<FieldParam> {
    match name {
        "key" => Some(FieldParam::Key),
        "title" => Some(FieldParam::Title),
        "tags" => Some(FieldParam::Tags),
        "coherent" => Some(FieldParam::Coherent),
        "chars" => Some(FieldParam::Chars),
        "distance" => Some(FieldParam::Distance),
        _ => None,
    }
}

/// The `coherent` filter value.
fn coherent_param(raw: &str) -> Result<CoherentFilter, String> {
    match raw {
        "true" => Ok(CoherentFilter::True),
        "false" => Ok(CoherentFilter::False),
        "null" => Ok(CoherentFilter::Null),
        "!true" => Ok(CoherentFilter::NotTrue),
        "!false" => Ok(CoherentFilter::NotFalse),
        "!null" => Ok(CoherentFilter::NotNull),
        other => Err(format!("bad `coherent`: {other}")),
    }
}

/// Render a positional `cols`/rows wire body as a header line and one line per row.
fn list_text(label: &str, wire: &str) -> String {
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

/// The first array-of-arrays under an object that also carries a `cols` header (searching nested
/// tables like `project_get`'s `preload`).
fn first_table(value: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
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

/// One wire cell as plain text (strings unquoted, arrays comma-joined).
fn cell(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items.iter().map(cell).collect::<Vec<_>>().join(","),
        serde_json::Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

/// The `GET /stats` prose: the headline counts, then the heads of the tag and usefulness lists.
fn stats_text(s: &store::stats::Stats) -> String {
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
        ("visits", s.visits_total.to_string()),
        ("useful", s.useful_total.to_string()),
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

/// The `GET /memory/{key}` prose: the header fields, then the body.
fn memory_text(mem: &store::Memory) -> String {
    let mut out = format!("{}\n", mem.key);
    out.push_str(&format!("  {:<8}{}\n", "title", mem.title));
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

/// How much of each `SELF` body `/bootstrap` carries: the doc's `?hydrate=keys|snippet|full`.
#[derive(Clone, Copy)]
enum BootHydrate {
    /// Keys, titles and sizes — the resident set is *this*, not the bodies.
    Keys,
    /// A one-line glance at each body.
    Snippet,
    /// Every body.
    Full,
}

impl BootHydrate {
    /// Parse the wire spelling; unknown values fall back to `keys` (never a 400 on a read).
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("snippet") => Self::Snippet,
            Some("full") => Self::Full,
            _ => Self::Keys,
        }
    }
}

/// One row of the `SELF` list.
struct BootKey {
    key: String,
    title: String,
    chars: usize,
    body: String,
}

/// The `/bootstrap` view, gathered once from a read-only connection and the writer witness.
struct BootData {
    report: crate::http::HealthReport,
    stamp: String,
    self_keys: Vec<BootKey>,
    boot_note: Option<BootKey>,
    open: Vec<(String, String)>,
    active: Vec<(String, String)>,
    hot: Vec<(String, i64, i64)>,
}

/// Gather the `/bootstrap` view from a **read-only connection** (never `Session`, so it cannot
/// queue behind a writer) plus the writer witness.
fn boot_data(state: &HttpState) -> BootData {
    let mut data = BootData {
        report: probe(&state.paths, &state.watch, state.started),
        stamp: String::new(),
        self_keys: Vec::new(),
        boot_note: None,
        open: Vec::new(),
        active: Vec::new(),
        hot: Vec::new(),
    };
    if !state.paths.db().exists() {
        return data;
    }
    let Ok(conn) = Connection::open_with_flags(state.paths.db(), OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return data;
    };
    data.stamp = utc_stamp(&conn).unwrap_or_default();
    if let Ok(rows) = memories_with_tag(&conn, "cline-self") {
        data.self_keys = rows
            .into_iter()
            .map(|(key, title, body)| BootKey {
                key,
                title,
                chars: body.chars().count(),
                body,
            })
            .collect();
    }
    data.boot_note = data
        .self_keys
        .iter()
        .find(|k| k.key == "bootstrap-self")
        .map(|k| BootKey {
            key: k.key.clone(),
            title: k.title.clone(),
            chars: k.chars,
            body: k.body.clone(),
        });
    if let Ok(rows) = memories_with_tag(&conn, "todo") {
        data.open = rows.into_iter().map(|(k, t, _)| (k, t)).collect();
    }
    if let Ok(mut stmt) = conn.prepare("SELECT key, title FROM project WHERE status = 'active'") {
        data.active = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .and_then(Iterator::collect)
            .unwrap_or_default();
    }
    if let Ok(mut stmt) = conn.prepare(
        "SELECT key, visits, times_useful FROM memory WHERE times_useful > 0
         ORDER BY times_useful DESC, key LIMIT 10",
    ) {
        data.hot = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .and_then(Iterator::collect)
            .unwrap_or_default();
    }
    data
}

/// `now` as the doc's ISO stamp (`YYYY-MM-DDTHH:MM:SSZ`), from the store's own clock.
fn utc_stamp(conn: &Connection) -> rusqlite::Result<String> {
    let raw: String = conn.query_row("SELECT datetime('now')", [], |r| r.get(0))?;
    Ok(format!("{}Z", raw.replace(' ', "T")))
}

/// Every memory carrying `tag`, as `(key, title, body)`, key order. Tags are a comma-joined
/// column, so the filter is applied in Rust over the decoded set.
fn memories_with_tag(
    conn: &Connection,
    tag: &str,
) -> rusqlite::Result<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare("SELECT key, title, body, tags FROM memory ORDER BY key")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (key, title, body, tags) = row?;
        if store::tags::decode(&tags).iter().any(|t| t == tag) {
            out.push((key, title, body));
        }
    }
    Ok(out)
}

/// `GET /bootstrap` — the boot payload in one read: health, the writer witness, the resident
/// `SELF` set, exactly one body (`bootstrap-self`), todos, active projects, hot, and the routes.
pub async fn bootstrap(State(state): State<HttpState>, RawQuery(raw): RawQuery) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let hydrate = BootHydrate::parse(params.get("hydrate"));
    let probe_state = state.clone();
    let data = tokio::task::spawn_blocking(move || boot_data(&probe_state)).await;
    match data {
        Ok(data) if json => render(
            StatusCode::OK,
            true,
            boot_json(&data).to_string(),
            String::new(),
        ),
        Ok(data) => render(
            StatusCode::OK,
            false,
            String::new(),
            boot_text(&data, hydrate),
        ),
        Err(_) => error(
            json,
            StatusCode::INTERNAL_SERVER_ERROR,
            "bootstrap task failed",
        ),
    }
}

/// The `/bootstrap` document (HTTP.md § "GET /bootstrap — the document").
fn boot_text(data: &BootData, hydrate: BootHydrate) -> String {
    let r = &data.report;
    let mut out = format!("BOOT ellmephant {}  {}\n", r.version, data.stamp);
    let ready = if r.ready {
        format!(
            "yes — projection {}, {}/{} vectors, schema v{}",
            r.projection, r.vectors, r.memories, r.schema
        )
    } else {
        format!("NO — {}", r.reason.as_deref().unwrap_or("unready"))
    };
    out.push_str(&format!("  {:<10}{ready}\n", "ready"));
    out.push_str(&format!("  {:<10}{}\n", "writer", r.writer));
    out.push_str(&format!(
        "  {:<10}{} memories, {} vectors, {} projects, {} links ({} dangling), {} tags\n",
        "store", r.memories, r.vectors, r.projects, r.links, r.links_dangling, r.tags
    ));

    out.push_str(&format!(
        "\nSELF (tag cline-self) — {} keys; bodies on demand: GET /memory/{{key}}\n",
        data.self_keys.len()
    ));
    for key in &data.self_keys {
        out.push_str(&format!(
            "  {:<18}{}    [{} chars]\n",
            key.key, key.title, key.chars
        ));
        match hydrate {
            BootHydrate::Keys => {}
            BootHydrate::Snippet => {
                out.push_str(&format!(
                    "    {}\n",
                    crate::search::snippet(&key.body, crate::search::SNIPPET_CHARS)
                ));
            }
            BootHydrate::Full => {
                for line in key.body.lines() {
                    out.push_str(&format!("    {line}\n"));
                }
            }
        }
    }

    out.push_str(&format!(
        "\nBOOT NOTE — {}\n",
        data.boot_note
            .as_ref()
            .map(|k| k.key.as_str())
            .unwrap_or("(none)")
    ));
    if let Some(note) = &data.boot_note {
        for line in note.body.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }

    out.push_str(&format!("\nOPEN (tag todo) — {}\n", data.open.len()));
    for (key, title) in &data.open {
        out.push_str(&format!("  {key} — {title}\n"));
    }

    out.push_str(&format!("\nACTIVE PROJECTS — {}\n", data.active.len()));
    for (key, title) in &data.active {
        out.push_str(&format!("  {key} — {title}\n"));
    }

    out.push_str("\nHOT — recently useful\n");
    for (key, visits, useful) in &data.hot {
        out.push_str(&format!("  {key} (visits {visits}, useful {useful})\n"));
    }

    out.push('\n');
    out.push_str(&routes_text());
    out
}

/// The `ROUTES` index, built from [`ROUTES`] so it cannot drift from the router.
fn routes_text() -> String {
    let mut lines: Vec<String> = Vec::new();
    for method in ["GET", "POST", "PUT", "DELETE"] {
        let mut paths: Vec<&str> = Vec::new();
        if method == "GET" {
            paths.push("/bootstrap");
            paths.push("/health");
        }
        for route in ROUTES {
            if route.method == method {
                paths.push(route.path);
            }
        }
        if paths.is_empty() {
            continue;
        }
        let auth = if method == "GET" {
            ""
        } else {
            "  (auth: bearer)"
        };
        lines.push(format!("  {method:<8}{}{auth}", paths.join("  ")));
    }
    format!("ROUTES\n{}\n", lines.join("\n"))
}

/// The `/bootstrap` view as JSON (`?format=json`) — the same sections as [`boot_text`], minus the
/// deliberately-absent bodies except the one resident note.
fn boot_json(data: &BootData) -> serde_json::Value {
    let r = &data.report;
    let self_keys: Vec<serde_json::Value> = data
        .self_keys
        .iter()
        .map(|k| serde_json::json!({ "key": k.key, "title": k.title, "chars": k.chars }))
        .collect();
    let boot_note = data
        .boot_note
        .as_ref()
        .map(|k| serde_json::json!({ "key": k.key, "body": k.body }));
    let rows = |items: &[(String, String)]| -> Vec<serde_json::Value> {
        items
            .iter()
            .map(|(key, title)| serde_json::json!({ "key": key, "title": title }))
            .collect()
    };
    let hot: Vec<serde_json::Value> = data
        .hot
        .iter()
        .map(|(key, visits, useful)| {
            serde_json::json!({ "key": key, "visits": visits, "times_useful": useful })
        })
        .collect();
    let mut routes: Vec<serde_json::Value> = vec![
        serde_json::json!({ "method": "GET", "path": "/health", "tool": null }),
        serde_json::json!({ "method": "GET", "path": "/bootstrap", "tool": null }),
    ];
    for route in ROUTES {
        routes.push(serde_json::json!({
            "method": route.method,
            "path": route.path,
            "tool": route.tool,
        }));
    }
    serde_json::json!({
        "version": r.version,
        "stamp": data.stamp,
        "ready": r.ready,
        "reason": r.reason,
        "writer": r.writer,
        "store": {
            "memories": r.memories,
            "vectors": r.vectors,
            "projects": r.projects,
            "links": r.links,
            "links_dangling": r.links_dangling,
            "tags": r.tags,
        },
        "self": self_keys,
        "boot_note": boot_note,
        "open": rows(&data.open),
        "active_projects": rows(&data.active),
        "hot": hot,
        "routes": routes,
    })
}
