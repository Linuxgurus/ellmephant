//! The full plain-HTTP surface (HTTP.md slice 2): every MCP tool as a route over the same
//! `Session`, plus `/bootstrap`.
//!
//! Three laws hold here, in order of strength. **Reads are pure — telemetry aside**: no `GET` writes
//! *data*, but a counter is not data, so `GET /memory/{key}` counts a `visits` read exactly as
//! `memory_get` does (HTTP.md invariant 1). **`/health` and `/bootstrap` never touch `Session`** —
//! they answer from their own read-only connection, so a read never queues behind a writer. **Auth
//! before writes** — every `POST`/`PUT`/`DELETE` resolves a bearer to a scope first, and a rejection
//! carries `WWW-Authenticate: Bearer` and nothing else.

use axum::body::{Body, to_bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;

use crate::http::HttpState;
use crate::mcp::{
    CoherentFilter, DUPES_LIMIT, FieldParam, GET_NUDGE, MatchModeParam, MemoryServer, SERVER_NAME,
    SearchParams, instructions, json_ok, tool_catalog,
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

/// The resolved authority of a request. Today one constant covers the store; per-area credentials
/// land later by resolving a different `Scope` in [`authorize`], and no handler changes.
#[derive(Clone, Copy)]
pub struct Scope {
    /// The namespace this credential may write.
    pub area: &'static str,
    /// The namespace, once per-area credentials exist (`None` = the whole store).
    pub namespace: Option<&'static str>,
}

/// The one scope every valid bearer resolves to today.
const STORE_SCOPE: Scope = Scope {
    area: "store",
    namespace: None,
};

/// The largest request body accepted (a memory, not a payload).
const MAX_BODY: usize = 4 * 1024 * 1024;

/// Resolve a credential → a scope, or `None`. The seam: handlers receive the scope, never the token.
///
/// **An unset server token means the write routes are OPEN.** That is deliberate and temporary:
/// requiring a bearer before there is any way to issue one would leave this surface read-only,
/// which is the one state it exists to end. Setting `ELLMEPHANT_WRITE_TOKEN` closes the door
/// again with no code change, so the gate stays a config flip rather than a rewrite. Read this
/// as a decision with an expiry date, not as the shape of the thing.
fn resolve(provided: Option<&str>, expected: Option<&str>) -> Option<Scope> {
    match (provided, expected) {
        (_, None) => Some(STORE_SCOPE),
        (Some(p), Some(e)) if p == e => Some(STORE_SCOPE),
        _ => None,
    }
}

/// The bearer on the request, if any.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Authorize a write: resolve the bearer, or hand back the bare 401 (no `resource_metadata`).
///
/// With no `ELLMEPHANT_WRITE_TOKEN` configured there is nothing to check and nothing to hand a
/// caller, so the door is open — see [`resolve`]. Configure the token and this becomes a gate.
#[allow(clippy::result_large_err)] // the Err is a ready-to-return response, not a value to carry
fn authorize(state: &HttpState, headers: &HeaderMap, json: bool) -> Result<Scope, Response> {
    resolve(bearer(headers), state.write_token()).ok_or_else(|| unauthorized(json))
}

/// The 401: `WWW-Authenticate: Bearer`, and **nothing more** — a `resource_metadata` challenge is
/// what makes a client invent an OAuth flow.
fn unauthorized(json: bool) -> Response {
    let mut response = error(json, StatusCode::UNAUTHORIZED, "unauthorized");
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

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

/// A parsed `text/plain` write: the named front matter and the verbatim body.
#[derive(Debug, Default)]
struct FrontMatter {
    key: Option<String>,
    title: Option<String>,
    tags: Option<Vec<String>>,
    source: Option<String>,
    body: String,
}

/// Parse the front matter: `Key:`/`Title:`/`Tags:`/`Source:` before the first `---`, the body after
/// it byte-for-byte with a trailing newline trimmed. An unknown field is an error that names it.
fn front_matter(text: &str) -> Result<FrontMatter, String> {
    let mut out = FrontMatter::default();
    let mut header_end = text.len();
    let mut body_start = text.len();
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "---" {
            header_end = pos;
            body_start = pos + line.len();
            break;
        }
        pos += line.len();
    }
    let body = &text[body_start..];
    out.body = body.strip_suffix('\n').unwrap_or(body).to_string();
    for line in text[..header_end].lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("malformed front-matter line `{line}`"));
        };
        let value = value.trim();
        match name.trim() {
            "Key" => out.key = Some(value.to_string()),
            "Title" => out.title = Some(value.to_string()),
            "Source" => out.source = Some(value.to_string()),
            "Tags" => {
                out.tags = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(str::to_string)
                        .collect(),
                );
            }
            other => return Err(format!("unknown front-matter field `{other}`")),
        }
    }
    Ok(out)
}

/// A `memory_upsert` patch from a text body (`Key:` required: it is the address).
fn memory_patch_text(text: &str) -> Result<store::Patch, String> {
    let fm = front_matter(text)?;
    let Some(key) = fm.key else {
        return Err("missing `Key:` front-matter field".to_string());
    };
    Ok(store::Patch {
        key: Some(key),
        title: fm.title,
        body: Some(fm.body),
        source: fm.source,
        tags: fm.tags,
    })
}

/// The JSON shape `POST /memory` accepts under `Content-Type: application/json`.
#[derive(serde::Deserialize)]
struct MemoryJson {
    key: Option<String>,
    title: Option<String>,
    body: Option<String>,
    source: Option<String>,
    tags: Option<Vec<String>>,
}

/// A `memory_upsert` patch from a JSON body.
fn memory_patch_json(bytes: &[u8]) -> Result<store::Patch, String> {
    let m: MemoryJson = serde_json::from_slice(bytes).map_err(|e| format!("bad JSON: {e}"))?;
    Ok(store::Patch {
        key: m.key,
        title: m.title,
        body: m.body,
        source: m.source,
        tags: m.tags,
    })
}

/// The JSON shape `POST /project` accepts under `Content-Type: application/json`.
#[derive(serde::Deserialize)]
struct ProjectJson {
    key: Option<String>,
    title: Option<String>,
    goal: Option<String>,
    state: Option<String>,
    status: Option<String>,
    owner: Option<Vec<String>>,
    tags: Option<Vec<String>>,
    preload: Option<Vec<String>>,
    start_date: Option<String>,
    end_date: Option<String>,
}

impl ProjectJson {
    /// Fold the JSON onto a partial project patch.
    fn into_patch(self) -> project::Patch {
        project::Patch {
            key: self.key,
            title: self.title,
            goal: self.goal,
            state: self.state,
            status: self.status,
            owner: self.owner,
            tags: self.tags,
            preload: self.preload,
            start_date: self.start_date,
            end_date: self.end_date,
        }
    }
}

/// A project patch from a JSON body.
fn project_patch_json(bytes: &[u8]) -> Result<project::Patch, String> {
    let p: ProjectJson = serde_json::from_slice(bytes).map_err(|e| format!("bad JSON: {e}"))?;
    Ok(p.into_patch())
}

/// A project patch from text front matter — the structured set is JSON's job, so text maps
/// `Key`/`Title`/`Tags` and carries the body as the project's `goal`.
fn project_patch_text(text: &str) -> Result<project::Patch, String> {
    let fm = front_matter(text)?;
    Ok(project::Patch {
        key: fm.key,
        title: fm.title,
        goal: (!fm.body.is_empty()).then_some(fm.body),
        tags: fm.tags,
        state: None,
        status: None,
        owner: None,
        preload: None,
        start_date: None,
        end_date: None,
    })
}

/// Read a content type: JSON only when the caller says so.
fn is_json_content(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("application/json"))
        .unwrap_or(false)
}

/// Collect a request body, erroring (as a 400) past [`MAX_BODY`].
async fn body_bytes(body: Body) -> Result<Vec<u8>, String> {
    to_bytes(body, MAX_BODY)
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("bad body: {e}"))
}

/// `GET /stats` — the whole-store digest (`memory_stats`).
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

/// `GET /memory/{key}` — one memory whole. Counts a `visits` read, exactly as `memory_get` does: the
/// one write a read may make, because a counter is not data (HTTP.md invariant 1).
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
    match server
        .with_store("memory_get", move |s| s.fetch(&key))
        .await
    {
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

/// `POST /memory` — store one memory (`memory_upsert`); text front-matter or JSON.
pub async fn upsert(
    State(state): State<HttpState>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let patch = if is_json_content(&headers) {
        memory_patch_json(&bytes)
    } else {
        memory_patch_text(&String::from_utf8_lossy(&bytes))
    };
    let patch = match patch {
        Ok(p) => p,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("memory_upsert", move |s| s.memory_merge(&patch))
        .await
    {
        Ok(out) => {
            tracing::info!(area = scope.area, key = %out.key, "memory stored over http");
            render(
                StatusCode::OK,
                json,
                json_ok(serde_json::json!({
                    "status": if out.created { "created" } else { "updated" },
                    "key": out.key,
                    "reembedded": out.reembedded,
                    "coherent_reset": out.coherent_reset,
                })),
                format!("stored {} (reembedded={})", out.key, out.reembedded),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `DELETE /memory/{key}` — remove one memory (`memory_delete`).
pub async fn memory_delete(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let lookup = key.clone();
    match server
        .with_store("memory_delete", move |s| s.rm(&lookup))
        .await
    {
        Ok(true) => {
            tracing::info!(area = scope.area, key = %key, "memory removed over http");
            render(
                StatusCode::OK,
                json,
                json_ok(serde_json::json!({ "status": "removed", "key": key })),
                format!("deleted {key}"),
            )
        }
        Ok(false) => error(
            json,
            StatusCode::NOT_FOUND,
            format!("no such memory: {key}"),
        ),
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /memory/{key}/visit` — the split-out counter: the write `GET` refused to be.
pub async fn visit(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let lookup = key.clone();
    match server
        .with_store("memory_visit", move |s| s.record_visit(&lookup))
        .await
    {
        Ok(visits) => {
            tracing::info!(area = scope.area, key = %key, visits, "visit counted over http");
            render(
                StatusCode::OK,
                json,
                json_ok(serde_json::json!({ "status": "visited", "key": key, "visits": visits })),
                format!("visited {key} (visits {visits})"),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /memory/{key}/upvote` — mark one memory useful (`memory_upvote`).
pub async fn upvote(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let sweep = vec![key.clone()];
    let lookup = key.clone();
    match server
        .with_store("memory_upvote", move |s| s.record_useful_many(&sweep))
        .await
    {
        Ok((upvoted, _missing)) => match upvoted.into_iter().next() {
            Some((_, times_useful)) => {
                tracing::info!(area = scope.area, key = %key, "upvoted over http");
                render(
                    StatusCode::OK,
                    json,
                    json_ok(serde_json::json!({
                        "status": "useful",
                        "upvoted": [{ "key": key, "times_useful": times_useful }],
                    })),
                    format!("upvoted {key} (useful {times_useful})"),
                )
            }
            None => error(
                json,
                StatusCode::NOT_FOUND,
                format!("no such memory: {lookup}"),
            ),
        },
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /memory/{key}/merge?into=` — retire one memory into another (`memory_merge`).
pub async fn merge(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(into) = params.maybe("into").map(str::to_string) else {
        return error(json, StatusCode::BAD_REQUEST, "missing `into` parameter");
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let from = key.clone();
    let target = into.clone();
    match server
        .with_store("memory_merge", move |s| s.merge_into(&from, &target))
        .await
    {
        Ok(out) => {
            tracing::info!(area = scope.area, from = %key, into = %into, "merged over http");
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&out).unwrap_or_default(),
                format!("merged {key} -> {into} ({} refs repointed)", out.bodies),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `PUT /memory/{key}/coherent?value=true|false|null` — the split-review verdict (`memory_coherent`).
pub async fn coherent(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let value = match params.get("value") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some("null") => None,
        Some(other) => {
            return error(
                json,
                StatusCode::BAD_REQUEST,
                format!("bad `value`: {other} (one of: true, false, null)"),
            );
        }
        None => return error(json, StatusCode::BAD_REQUEST, "missing `value` parameter"),
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let mark = key.clone();
    match server
        .with_store("memory_coherent", move |s| s.mark_coherent(&mark, value))
        .await
    {
        Ok(()) => {
            tracing::info!(area = scope.area, key = %key, coherent = ?value, "marked over http");
            render(
                StatusCode::OK,
                json,
                json_ok(serde_json::json!({ "status": "marked", "key": key, "coherent": value })),
                format!(
                    "marked {key} (coherent={})",
                    value.map_or("null", |v| if v { "true" } else { "false" })
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /reembed` — recompute every vector (`memory_reembed`); the heaviest lock holder, so last
/// in the route table and watched by `/health`.
pub async fn reembed(
    State(state): State<HttpState>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("memory_reembed", move |s| s.reembed())
        .await
    {
        Ok(report) => {
            tracing::info!(
                area = scope.area,
                rows = report.rows,
                "re-embedded over http"
            );
            render(
                StatusCode::OK,
                json,
                json_ok(serde_json::json!({
                    "status": "re-embedded",
                    "rows": report.rows,
                    "dim": report.dim,
                    "rebuilt": report.rebuilt,
                    "model": report.model,
                })),
                format!(
                    "re-embedded {} rows ({} rebuilt)",
                    report.rows, report.rebuilt
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /project` — create or replace a project (`project_upsert`); JSON or text front-matter.
pub async fn project_upsert(
    State(state): State<HttpState>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let patch = if is_json_content(&headers) {
        project_patch_json(&bytes)
    } else {
        project_patch_text(&String::from_utf8_lossy(&bytes))
    };
    let patch = match patch {
        Ok(p) => p,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("project_upsert", move |s| s.project_merge(&patch))
        .await
    {
        Ok(out) => {
            tracing::info!(area = scope.area, key = %out.key, "project stored over http");
            render(
                StatusCode::OK,
                json,
                json_ok(serde_json::json!({
                    "status": if out.created { "created" } else { "updated" },
                    "key": out.key,
                })),
                format!(
                    "stored project {} ({})",
                    out.key,
                    if out.created { "created" } else { "updated" }
                ),
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

/// One tool as the boot presents it: the MCP card with an HTTP verb and path where the MCP tool
/// name was. Every other field is the MCP field unchanged — same description, same schema, same
/// annotations — because the requirement is that changing transport changes the verbs and nothing
/// else.
struct BootTool {
    /// The HTTP verb that reaches this tool.
    method: &'static str,
    /// The path template; `{key}` is the placeholder.
    path: &'static str,
    /// The law, verbatim from the tool.
    description: String,
    /// The argument schema, verbatim from the tool.
    input_schema: serde_json::Value,
    /// The annotation hints, as `tools/list` serializes them.
    annotations: serde_json::Value,
}

/// The tool surface, each MCP tool paired with the route that reaches it.
///
/// The cards come from the MCP server's own router and the verbs from [`ROUTES`], so the boot
/// cannot describe a toolkit the doors do not have: a tool the router carries but no route reaches
/// is dropped here, and `every_tool_has_a_route` fails the suite for it.
fn boot_tools() -> Vec<BootTool> {
    tool_catalog()
        .into_iter()
        .filter_map(|card| {
            let route = ROUTES.iter().find(|route| route.tool == card.name)?;
            Some(BootTool {
                method: route.method,
                path: route.path,
                description: card.description,
                input_schema: card.input_schema,
                annotations: card.annotations,
            })
        })
        .collect()
}

/// `GET /bootstrap` — the boot document: exactly what an MCP client is handed at `initialize` and
/// `tools/list`, with HTTP verbs in place of MCP verbs, and nothing else.
///
/// **Nothing else is the requirement, not a limitation.** This is the store's *API surface*, not a
/// view of its contents: the resident set (self, todos, active projects, hot) is one of the
/// commands below, exactly as it is over MCP, and `/health` answers readiness.
///
/// Because the document is built from code rather than from the store, it needs no database and
/// takes no lock — so a boot still describes the API when the store is missing or stalled, which is
/// exactly when a caller needs to know what it can ask for.
pub async fn bootstrap(RawQuery(raw): RawQuery) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let tools = boot_tools();
    render(
        StatusCode::OK,
        json,
        boot_json(&tools).to_string(),
        boot_text(&tools),
    )
}

/// The boot document in the store's text form: the same information as [`boot_json`], rendered for
/// a model to read rather than a machine to parse.
fn boot_text(tools: &[BootTool]) -> String {
    let mut out = format!(
        "BOOT {SERVER_NAME} {}   capabilities: tools\n",
        crate::VERSION
    );
    out.push_str(&format!(
        "\nINSTRUCTIONS\n{}\n",
        indent_block(instructions(), "  ")
    ));
    out.push_str(&format!(
        "\nTOOLS — {}; HTTP verbs in place of MCP verbs, arguments and descriptions unchanged\n",
        tools.len()
    ));
    for tool in tools {
        out.push_str(&format!("\n  {} {}\n", tool.method, tool.path));
        out.push_str(&format!("      {}\n", tool.description.replace('\n', " ")));
        if let Some(args) = args_text(&tool.input_schema) {
            out.push_str(&format!("      args: {args}\n"));
        }
    }
    out
}

/// A tool's arguments, one line, read from the tool's own schema — so the text form cannot drift
/// from the schema the MCP door hands a model.
fn args_text(schema: &serde_json::Value) -> Option<String> {
    let props = schema.get("properties")?.as_object()?;
    if props.is_empty() {
        return None;
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let parts: Vec<String> = props
        .iter()
        .map(|(name, spec)| {
            let need = if required.contains(&name.as_str()) {
                "required"
            } else {
                "optional"
            };
            let kind = schema_type(spec, schema);
            match spec.get("description").and_then(|d| d.as_str()) {
                Some(description) if !description.is_empty() => {
                    format!("{name}: {kind} ({need}) — {description}")
                }
                _ => format!("{name}: {kind} ({need})"),
            }
        })
        .collect();
    Some(parts.join("; "))
}

/// A property's type as a reader wants it: scalars, arrays of them, and `enum` value sets.
///
/// A nullable property is reported by the type it actually carries — `k` is an integer you may
/// omit, not "any" — and a `$ref` is resolved against the tool's own `$defs`, so an enum shows the
/// values a caller may actually send.
fn schema_type(spec: &serde_json::Value, root: &serde_json::Value) -> String {
    let resolved = deref(spec, root);
    if let Some(real) = resolved
        .get("anyOf")
        .and_then(|b| b.as_array())
        .and_then(|branches| {
            branches
                .iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) != Some("null"))
        })
    {
        return schema_type(real, root);
    }
    if let Some(values) = resolved.get("enum").and_then(|e| e.as_array()) {
        return values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("|");
    }
    // A unit-only enum is spelled `oneOf` of `const`s (`literal` | `regex`), one entry per variant.
    if let Some(branches) = resolved.get("oneOf").and_then(|o| o.as_array()) {
        let consts: Vec<String> = branches
            .iter()
            .filter_map(|branch| branch.get("const"))
            .map(ToString::to_string)
            .collect();
        if !consts.is_empty() && consts.len() == branches.len() {
            return consts.join("|");
        }
        if let Some(first) = branches.first() {
            return schema_type(first, root);
        }
    }
    match non_null_type(resolved).as_deref() {
        Some("array") => {
            let inner = resolved
                .get("items")
                .map_or_else(|| "any".to_string(), |item| schema_type(item, root));
            // `"a"|"b"[]` would read as if only the last value were the array.
            if inner.contains('|') {
                format!("({inner})[]")
            } else {
                format!("{inner}[]")
            }
        }
        Some(other) => other.to_string(),
        None => "any".to_string(),
    }
}

/// Follow a `$ref` one hop against the schema's own `$defs`. A ref that does not resolve is left
/// alone rather than guessed at.
fn deref<'a>(spec: &'a serde_json::Value, root: &'a serde_json::Value) -> &'a serde_json::Value {
    let name = spec
        .get("$ref")
        .and_then(|r| r.as_str())
        .and_then(|r| r.strip_prefix("#/$defs/"));
    name.and_then(|name| root.get("$defs").and_then(|defs| defs.get(name)))
        .unwrap_or(spec)
}

/// The type a property actually carries: a `type` may be a name, or an array of them when the
/// property is nullable (`["integer", "null"]` is an optional integer).
fn non_null_type(spec: &serde_json::Value) -> Option<String> {
    match spec.get("type")? {
        serde_json::Value::String(name) => (name != "null").then(|| name.clone()),
        serde_json::Value::Array(names) => names
            .iter()
            .filter_map(|n| n.as_str())
            .find(|n| *n != "null")
            .map(ToString::to_string),
        _ => None,
    }
}

/// Prefix every line of `text` with `pad`, so a block reads as one section.
fn indent_block(text: &str, pad: &str) -> String {
    text.lines()
        .map(|line| format!("{pad}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The boot document as `?format=json` — the MCP handshake's own shape.
///
/// The fields are `initialize`'s (`serverInfo`, `capabilities`, `instructions`) plus `tools/list`'s
/// (`tools`), and each tool carries `method`/`path` **where MCP carried `name`**. Everything else on
/// a card — `title`, `description`, `inputSchema`, `annotations` — is the MCP field untouched.
///
/// `protocolVersion` is the one field an MCP boot has that this does not: it names a *transport*
/// version, and over HTTP there is no MCP protocol to negotiate. Left out rather than faked.
fn boot_json(tools: &[BootTool]) -> serde_json::Value {
    let tools: Vec<serde_json::Value> = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "method": tool.method,
                "path": tool.path,
                "description": tool.description,
                "inputSchema": tool.input_schema,
                "annotations": tool.annotations,
            })
        })
        .collect();
    serde_json::json!({
        "serverInfo": { "name": SERVER_NAME, "version": crate::VERSION },
        "capabilities": { "tools": {} },
        "instructions": instructions(),
        "tools": tools,
    })
}
