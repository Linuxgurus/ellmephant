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

use crate::config::Paths;
use crate::db;
use crate::http::HttpState;
use crate::mcp::{
    DUPES_LIMIT, FieldParam, GET_NUDGE, HISTORY_LIMIT, MatchModeParam, MemoryServer, SERVER_NAME,
    SearchParams, SemanticSearchParams, instructions, json_ok, tool_catalog,
};
use crate::search::MatchMode;
use crate::store::{
    self, encode,
    links::{backlinks_to_wire_json, dangling_to_wire_json},
    project, tags,
};
use rusqlite::OptionalExtension;

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
        method: "GET",
        path: "/whoami",
        tool: "whoami",
    },
    Route {
        method: "GET",
        path: "/whois/{pen}",
        tool: "whois",
    },
    Route {
        method: "POST",
        path: "/pen/mint",
        tool: "pen_mint",
    },
    Route {
        method: "POST",
        path: "/pen/lend",
        tool: "pen_lend",
    },
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
        path: "/semantic",
        tool: "semantic_search",
    },
    Route {
        method: "GET",
        path: "/memory/{key}",
        tool: "memory_get",
    },
    Route {
        method: "GET",
        path: "/history",
        tool: "memory_history",
    },
    Route {
        method: "GET",
        path: "/memory/{key}/backlinks",
        tool: "memory_backlinks",
    },
    Route {
        method: "GET",
        path: "/dangling",
        tool: "memory_dangling",
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
        path: "/reparse",
        tool: "memory_reparse",
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
    Route {
        method: "POST",
        path: "/claim",
        tool: "claim_acquire",
    },
    Route {
        method: "POST",
        path: "/claim/{key}/release",
        tool: "claim_release",
    },
    Route {
        method: "GET",
        path: "/claim",
        tool: "claim_list",
    },
    Route {
        method: "POST",
        path: "/mail",
        tool: "mail_send",
    },
    Route {
        method: "GET",
        path: "/mail",
        tool: "mail_inbox",
    },
    Route {
        method: "GET",
        path: "/mail/{seq}",
        tool: "mail_read",
    },
    Route {
        method: "POST",
        path: "/mail/ack",
        tool: "mail_ack",
    },
    Route {
        method: "POST",
        path: "/mail/prune",
        tool: "mail_prune",
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

/// 404 for a store error that names a missing row; 413 for a body past the store's cap; 503 for a
/// derived index that is not there; 400 otherwise (a malformed request).
fn error_status(message: &str) -> StatusCode {
    if message.starts_with("no such memory:") || message.starts_with("no such project:") {
        StatusCode::NOT_FOUND
    } else if message.starts_with("body too long:") {
        // The request is well formed and the caller is welcome; it is simply too big to hold.
        StatusCode::PAYLOAD_TOO_LARGE
    } else if message.starts_with("index unavailable:") {
        // Not the caller's fault and not permanent: the derived index needs rebuilding.
        StatusCode::SERVICE_UNAVAILABLE
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
    origin: Option<String>,
    version_note: Option<String>,
    private: Option<bool>,
    body: String,
}

/// Parse the front matter: `Key:`/`Title:`/`Tags:`/`Source:`/`Note:` before the first `---`, the body
/// after it byte-for-byte with a trailing newline trimmed. An unknown field is an error that names it.
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
            "Origin" => out.origin = Some(value.to_string()),
            "Note" => out.version_note = Some(value.to_string()),
            "Private" => {
                out.private = Some(match value.to_ascii_lowercase().as_str() {
                    "true" | "yes" | "1" => true,
                    "false" | "no" | "0" | "" => false,
                    other => return Err(format!("`Private: {other}` is not true|false")),
                })
            }
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
///
/// Returns the patch and the pen to CREDIT, if any — `None` for the text door, where a front-matter
/// `As:` field would be a second spelling of the JSON `as` with no caller asking for it yet.
fn memory_patch_text(text: &str) -> Result<(store::Patch, Option<String>), String> {
    let fm = front_matter(text)?;
    let Some(key) = fm.key else {
        return Err("missing `Key:` front-matter field".to_string());
    };
    Ok((
        store::Patch {
            key: Some(key),
            title: fm.title,
            body: Some(fm.body),
            source: fm.source,
            // A text body carries no speaker: the handler stamps `asserted_by` from the request.
            asserted_by: None,
            origin: fm.origin,
            tags: fm.tags,
            private: fm.private,
            version_note: fm.version_note,
            expected_version: None,
        },
        None,
    ))
}

/// The JSON shape `POST /memory` accepts under `Content-Type: application/json`.
#[derive(serde::Deserialize)]
struct MemoryJson {
    key: Option<String>,
    title: Option<String>,
    body: Option<String>,
    source: Option<String>,
    origin: Option<String>,
    tags: Option<Vec<String>>,
    version_note: Option<String>,
    /// Credit this write to a descendant of the calling pen (`as`), then validated against a live
    /// lend. Both doors offer the same argument, or the door is a second, weaker store.
    #[serde(default, rename = "as")]
    as_pen: Option<String>,
    /// Keep this row to the writing pen alone.
    private: Option<bool>,
    /// The CAS token, as `memory_get` returns it.
    expected_version: Option<i64>,
}

/// A `memory_upsert` patch from a JSON body.
fn memory_patch_json(bytes: &[u8]) -> Result<(store::Patch, Option<String>), String> {
    let m: MemoryJson = serde_json::from_slice(bytes).map_err(|e| format!("bad JSON: {e}"))?;
    let as_pen = m.as_pen;
    Ok((
        store::Patch {
            key: m.key,
            title: m.title,
            body: m.body,
            source: m.source,
            asserted_by: None,
            origin: m.origin,
            tags: m.tags,
            private: m.private,
            version_note: m.version_note,
            expected_version: m.expected_version,
        },
        as_pen,
    ))
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

/// The request's JSON body, when it sent one — `None` for an empty body or a non-JSON content type.
///
/// This is what lets a route take its arguments **the way the MCP tool does** (`{"into": ...}`)
/// *and* its own query parameters, so a caller that knows the tool can post the
/// tool's own arguments unchanged. Parity is the point of this door; guessing at the shape is not.
async fn json_body(headers: &HeaderMap, body: Body) -> Result<Option<serde_json::Value>, String> {
    if !is_json_content(headers) {
        return Ok(None);
    }
    let bytes = body_bytes(body).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("bad json body: {e}"))
}

/// `POST /pen/lend` — lend this request's pen's authority down to a descendant (`pen_lend`).
pub async fn pen_lend(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let caller = match crate::identity::resolve(&headers) {
        Ok(Some(name)) => name,
        Ok(None) => {
            return error(
                json,
                StatusCode::UNAUTHORIZED,
                "refused: lending needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`",
            );
        }
        Err(forged) => return error(json, StatusCode::UNAUTHORIZED, forged.to_string()),
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        pen: String,
        secs: i64,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, format!("bad JSON: {e}")),
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("pen_lend", move |s| s.lend(&caller, &ask.pen, ask.secs))
        .await
    {
        Ok(session) => {
            tracing::info!(
                area = scope.area,
                weak = %session.weak,
                powerful = %session.powerful,
                "authority lent down over http"
            );
            let value = serde_json::to_value(&session).unwrap_or(serde_json::Value::Null);
            render(
                StatusCode::CREATED,
                json,
                value.to_string(),
                format!(
                    "lent {} -> {} until {}\n",
                    session.powerful, session.weak, session.expires_at
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /pen/mint` — create a pen under this request's pen (`pen_mint`).
pub async fn pen_mint(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    // A mint is a WRITE: the caller is the parent, and an unproven caller is nobody.
    let caller = match crate::identity::resolve(&headers) {
        Ok(Some(name)) => name,
        Ok(None) => {
            return error(
                json,
                StatusCode::UNAUTHORIZED,
                "refused: minting needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`",
            );
        }
        Err(forged) => return error(json, StatusCode::UNAUTHORIZED, forged.to_string()),
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        name: String,
        kind: String,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, format!("bad JSON: {e}")),
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("pen_mint", move |s| s.mint(&caller, &ask.name, &ask.kind))
        .await
    {
        Ok(minted) => {
            tracing::info!(
                area = scope.area,
                pen = %minted.name,
                parent = %minted.parent,
                "pen minted over http"
            );
            let value = serde_json::to_value(&minted).unwrap_or(serde_json::Value::Null);
            render(
                StatusCode::CREATED,
                json,
                value.to_string(),
                format!(
                    "minted {} ({}) under {}\n",
                    minted.name, minted.kind, minted.parent
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /whois/{pen}` — one pen and what it is to this request's pen (`whois`).
pub async fn whois(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(pen): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let viewer = crate::identity::resolve(&headers)
        .ok()
        .flatten()
        .unwrap_or_else(|| crate::identity::ANONYMOUS.to_string());
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_read("whois", reader_from(&headers), move |s| {
            s.whois(&viewer, &pen)
        })
        .await
    {
        Ok(facts) => {
            let value = serde_json::to_value(&facts).unwrap_or(serde_json::Value::Null);
            let text = format!(
                "{}\n  {:<10}{}\n  {:<10}{}\n  {:<10}{}\n  {:<10}{}\n  {:<10}{} row(s)\n  {:<10}{}\n",
                facts.name,
                "kind",
                facts.kind,
                "parent",
                facts.parent.as_deref().unwrap_or("(root)"),
                "created",
                facts.created,
                "state",
                facts
                    .ceased
                    .as_deref()
                    .map(|c| format!("retired {c}"))
                    .unwrap_or_else(|| "live".to_string()),
                "owns",
                facts.owns,
                "relation",
                facts.relation.as_deref().unwrap_or("(no common root)"),
            );
            render(StatusCode::OK, json, value.to_string(), text)
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /whoami` — the identity this request would be stamped with, and whether it is PROVEN.
/// Touches no store and changes nothing: the REST twin of the MCP `whoami`, so a `curl` can ask the
/// same question the model asks instead of guessing from a remembered flag.
pub async fn whoami(headers: HeaderMap) -> Response {
    let (doc, text) = match crate::identity::resolve(&headers) {
        Ok(Some(name)) => {
            let text = format!("{name} (proven)\n");
            (
                serde_json::json!({ "asserted_by": name, "proven": true }),
                text,
            )
        }
        Ok(None) => (
            serde_json::json!({ "asserted_by": crate::identity::ANONYMOUS, "proven": false }),
            format!("{} (not proven)\n", crate::identity::ANONYMOUS),
        ),
        // A claim that failed verification is still worth reporting: the caller learns the name it
        // sent is not the name it has. The secret is never echoed.
        Err(forged) => {
            let claimed = forged.0;
            (
                serde_json::json!({
                    "asserted_by": crate::identity::ANONYMOUS,
                    "proven": false,
                    "claimed": claimed,
                    "problem": "the X-Stet-Auth proof is missing or wrong",
                }),
                format!(
                    "{} (not proven; claimed `{claimed}`)\n",
                    crate::identity::ANONYMOUS
                ),
            )
        }
    };
    render(StatusCode::OK, true, doc.to_string(), text)
}

/// Who this request reads as. A proven pair decides; anything else — including a forged claim — is
/// nobody, and nobody sees public rows only. Reads never fail for want of a pen.
fn reader_from(headers: &HeaderMap) -> crate::store::audience::Reader {
    match crate::identity::resolve(headers) {
        Ok(Some(name)) => crate::store::audience::Reader::pen(&name),
        _ => crate::store::audience::Reader::anonymous(),
    }
}

/// `GET /stats` — the whole-store digest (`memory_stats`).
pub async fn stats(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let with_tags = params.flag("tags");
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_read("memory_stats", reader, move |s| s.stats(with_tags))
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
    headers: HeaderMap,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let version = match params.parse_as::<i64>("version") {
        Ok(v) => v,
        Err(msg) => return error(json, StatusCode::BAD_REQUEST, msg),
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    // `?version=N` reads a PAST state: by address, so the live row need not exist, and counting no
    // visit — `visits` measures the memory, not a state the memory held.
    if let Some(n) = version {
        let lookup = key.clone();
        return match server
            .with_read("memory_get", reader.clone(), move |s| s.version(&lookup, n))
            .await
        {
            Ok(Some(past)) => {
                let mut value = serde_json::to_value(&past).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("version".to_string(), serde_json::json!(n));
                    obj.insert("_nudge".to_string(), serde_json::json!(GET_NUDGE));
                }
                // A PAST state is read by address: its owner rides inside `content`, and a walk is
                // for a pen that still answers, so no relation is printed here.
                render(
                    StatusCode::OK,
                    json,
                    value.to_string(),
                    memory_text(&past, None),
                )
            }
            Ok(None) => error(
                json,
                StatusCode::NOT_FOUND,
                format!("no such version: {key} {n}"),
            ),
            Err(e) => error(json, error_status(&e), e),
        };
    }
    let lookup = key.clone();
    match server
        .with_read("memory_get", reader, move |s| {
            let mem = s.show(&lookup)?;
            // The relation belongs to the READER, so it is computed where the proven pen is known.
            let relation = s.owner_relation(&mem);
            Ok((mem, relation))
        })
        .await
    {
        Ok((mut mem, relation)) => {
            // Count the read best-effort (see `MemoryServer::bump_visit`): telemetry never makes a
            // reader wait, so a write in flight means this visit goes uncounted.
            if let Some(seen) = server.bump_visit(&key) {
                mem.visits = seen;
            }
            let mut value = serde_json::to_value(&mem).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = value.as_object_mut() {
                obj.insert("_nudge".to_string(), serde_json::json!(GET_NUDGE));
                if let Some(walk) = &relation {
                    obj.insert("owner_relation".to_string(), serde_json::json!(walk));
                }
            }
            render(
                StatusCode::OK,
                json,
                value.to_string(),
                memory_text(&mem, relation.as_deref()),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /memory/{key}/backlinks` — incoming `[[…]]` edges (`memory_backlinks`).
pub async fn backlinks(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers);
    let json = Params::parse(raw.as_deref()).json();
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_read("memory_backlinks", reader, move |s| s.backlinks(&key))
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

/// `GET /dangling` — every broken `[[…]]` edge, one row per referrer (`memory_dangling`).
pub async fn dangling(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let reader = reader_from(&headers);
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_read("memory_dangling", reader, |s| s.dangling())
        .await
    {
        Ok(rows) => {
            let wire = dangling_to_wire_json(&rows);
            render(
                StatusCode::OK,
                json,
                wire.clone(),
                list_text("dangling", &wire),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /history` — one key's versions (`?key=…`), or the index of every key holding one.
///
/// A TOP-LEVEL path on purpose: history is not a sub-resource of a live memory. The case it exists
/// for — an orphan, whose row is gone — has no `/memory/{key}` to hang from, and a path that implied
/// the memory still existed would be wrong for exactly the caller who needs it most.
pub async fn history(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let cap = match params.parse_as::<u32>("limit") {
        Ok(v) => v.unwrap_or(HISTORY_LIMIT as u32) as usize,
        Err(msg) => return error(json, StatusCode::BAD_REQUEST, msg),
    };
    let orphans_only = params.flag("orphans_only");
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match params.maybe("key") {
        Some(key) => {
            let lookup = key.to_string();
            match server
                .with_read("memory_history", reader.clone(), move |s| {
                    s.versions(&lookup)
                })
                .await
            {
                Ok(rows) => {
                    let total = rows.len();
                    let wire = serde_json::json!({
                        "key": key,
                        "count": total,
                        "versions": rows.into_iter().take(cap).collect::<Vec<_>>(),
                    })
                    .to_string();
                    render(
                        StatusCode::OK,
                        json,
                        wire.clone(),
                        list_text("versions", &wire),
                    )
                }
                Err(e) => error(json, error_status(&e), e),
            }
        }
        // No key: the INDEX. Nothing else lists keys that are gone, so this is how an orphan is found.
        None => match server
            .with_read("memory_history", reader, move |s| {
                s.version_index(orphans_only)
            })
            .await
        {
            Ok(rows) => {
                let total = rows.len();
                let wire = serde_json::json!({
                    "count": total,
                    "keys": rows.into_iter().take(cap).collect::<Vec<_>>(),
                })
                .to_string();
                render(
                    StatusCode::OK,
                    json,
                    wire.clone(),
                    list_text("history", &wire),
                )
            }
            Err(e) => error(json, error_status(&e), e),
        },
    }
}

/// `GET /search` — the same query surface as `memory_search`, same bytes.
pub async fn search(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers);
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
    match server.run_search(search, reader).await {
        Ok(wire) => render(StatusCode::OK, json, wire.clone(), list_text("hits", &wire)),
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /semantic` — the store-wide search (`semantic_search`): an optional `type`, and
/// `deleted=true` for the graveyard. Same body as the tool, by construction.
pub async fn semantic_search(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let mut fields = Vec::new();
    if let Some(raw) = params.maybe("fields") {
        for name in raw.split(',') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            match field_param(name) {
                Some(f) => fields.push(f),
                None => {
                    return error(
                        json,
                        StatusCode::BAD_REQUEST,
                        format!("bad `fields`: {name}"),
                    );
                }
            }
        }
    }
    let p = SemanticSearchParams {
        query: params.maybe("q").map(str::to_string),
        doc_type: params.maybe("type").map(str::to_string),
        deleted: params
            .maybe("deleted")
            .map(|v| matches!(v, "1" | "true" | "yes")),
        k: params.maybe("k").and_then(|v| v.parse().ok()),
        max_distance: params.maybe("max_distance").and_then(|v| v.parse().ok()),
        fields,
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server.run_semantic(p, reader).await {
        Ok(wire) => render(StatusCode::OK, json, wire.clone(), list_text("hits", &wire)),
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /dupes` — near-duplicate candidate pairs (`memory_dupes`).
pub async fn dupes(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers);
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
        .with_read("memory_dupes", reader, move |s| {
            s.dupes(max_distance, limit)
        })
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
pub async fn projects(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers);
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
        .with_read("project_list", reader, move |s| s.project_list(&filter))
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
    headers: HeaderMap,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers);
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
        .with_read("project_get", reader, move |s| {
            s.project_view(&key, hydrate)
        })
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

/// `POST /claim` — take or renew an atomic lease (`claim_acquire`); JSON body of the tool's args.
pub async fn claim_acquire(
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
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        Ok(None) => return error(json, StatusCode::BAD_REQUEST, "a JSON body is required"),
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let name = match doc.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return error(json, StatusCode::BAD_REQUEST, "missing `name`"),
    };
    let owner = match doc.get("owner").and_then(|v| v.as_str()) {
        Some(o) => o.to_string(),
        None => return error(json, StatusCode::BAD_REQUEST, "missing `owner`"),
    };
    let ttl = doc.get("ttl_seconds").and_then(|v| v.as_i64());
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("claim_acquire", move |s| {
            s.claim_acquire(&name, &owner, ttl)
        })
        .await
    {
        Ok(out) => {
            tracing::info!(
                area = scope.area,
                name = %out.claim.name,
                owner = %out.claim.owner,
                won = out.won,
                "claim over http"
            );
            let text = if out.won {
                format!(
                    "claimed `{}` for {} until {}",
                    out.claim.name, out.claim.owner, out.claim.expires_at
                )
            } else {
                format!(
                    "refused: `{}` held by {} until {}",
                    out.claim.name, out.claim.owner, out.claim.expires_at
                )
            };
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&out).unwrap_or_default(),
                text,
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /claim/{key}/release` — drop your own lease (`claim_release`). `owner` from body or query.
pub async fn claim_release(
    State(state): State<HttpState>,
    Path(name): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let doc = match json_body(&headers, body).await {
        Ok(v) => v,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let owner = doc
        .as_ref()
        .and_then(|v| v.get("owner"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| params.maybe("owner").map(str::to_string));
    let Some(owner) = owner else {
        return error(json, StatusCode::BAD_REQUEST, "missing `owner`");
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("claim_release", move |s| s.claim_release(&name, &owner))
        .await
    {
        Ok(out) => {
            tracing::info!(
                area = scope.area,
                released = out.released,
                "claim release over http"
            );
            let text = if out.released {
                "released".to_string()
            } else {
                match &out.claim {
                    Some(c) => format!("refused: held by {} until {}", c.owner, c.expires_at),
                    None => "refused: no such claim".to_string(),
                }
            };
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&out).unwrap_or_default(),
                text,
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /claim` — every claim, live or expired (`claim_list`).
pub async fn claims(State(state): State<HttpState>, RawQuery(raw): RawQuery) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let name = params.maybe("name").map(str::to_string);
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_read(
            "claim_list",
            crate::store::audience::Reader::everything(),
            move |s| s.claims(name.as_deref()),
        )
        .await
    {
        Ok(rows) => {
            let text = if rows.is_empty() {
                "no claims".to_string()
            } else {
                rows.iter()
                    .map(|c| {
                        format!(
                            "{} {} held by {} (until {})",
                            if c.live { "live" } else { "expired" },
                            c.name,
                            c.owner,
                            c.expires_at
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&serde_json::json!({ "claims": rows })).unwrap_or_default(),
                text,
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
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    // `?wait=true`: settle this document before answering, so the caller can search the moment it has
    // a reply. Opt-in, because it is the one path that holds the store lock across the model call.
    let wait = matches!(params.get("wait"), Some("true") | Some("1"));
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
    let (mut patch, as_pen) = match patch {
        Ok(p) => p,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    // ★ THE COMMENT ABOVE WAS TRUE AND THE CODE WAS NOT: the parsers return `asserted_by: None` and
    // nobody stamped it, so every row written over HTTP landed in the COMMONS — owner NULL — and
    // since #119 that means any pen may rewrite it. The bearer proves the door; the identity pair
    // names the pen, and only a PROVEN one is stamped (#109).
    let proven = crate::identity::resolve(&headers).ok().flatten();
    // ★ A SILENTLY IGNORED ARGUMENT IS A LIE ABOUT WHAT HAPPENED (found reviewing #126): with no
    // proven pen there is nobody to check the claim against, so `as` would be dropped and the write
    // attributed to nobody while the caller believed the finger was credited.
    if as_pen.is_some() && proven.is_none() {
        return error(
            json,
            StatusCode::UNAUTHORIZED,
            "refused: crediting a descendant needs a PROVEN pen — send `X-Stet-From` and `X-Stet-Auth`",
        );
    }
    let credited = match (&as_pen, &proven) {
        (Some(claimed), Some(caller)) if !claimed.eq_ignore_ascii_case(caller) => {
            Some(claimed.clone())
        }
        _ => None,
    };
    patch.asserted_by = credited.clone().or_else(|| proven.clone());
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("memory_upsert", move |s| {
            // ★ A credit needs a LIVE LEND, exactly as on the MCP door: `as` is not a rename of the
            // writer, it is a claim that this pen may act for that one, and parentage alone is not
            // enough to make it true.
            if let (Some(as_pen), Some(caller)) = (&credited, &proven) {
                s.may_act_as(caller, as_pen)?;
            }
            s.memory_merge(&patch)
        })
        .await
    {
        Ok(out) => {
            // ★ A REFUSED COMPARE-AND-SWAP WRITE IS NOT A SUCCESS (#101). `expected_version` is the
            // documented way to do read-modify-write ("a bank balance, a counter"), so a 200
            // `updated` for a write that did not land is a silent failure: the reconcile loop
            // believes it reconciled. Answer 409 with the SAME body the MCP tool returns, so a
            // client that only checks the code is right too, and one that reads the body sees the
            // version to retry against.
            if let Some(current) = out.conflict {
                let key = out.key;
                tracing::info!(
                    area = scope.area,
                    key = %key,
                    version = current,
                    "compare-and-swap write refused over http"
                );
                return render(
                    StatusCode::CONFLICT,
                    json,
                    json_ok(serde_json::json!({
                        "status": "conflict",
                        "key": key,
                        "current_version": current,
                    })),
                    format!("conflict: {key} is at version {current}; nothing written"),
                );
            }
            let key = out.key.clone();
            let mut result = serde_json::json!({
                "status": if out.created { "created" } else { "updated" },
                "key": key.clone(),
                "queued": out.queued,
            });
            // Filled in below when `?wait=true` was asked for.
            let mut indexed: Option<bool> = None;
            if wait {
                indexed = Some(match server.encode_now(&out.key).await {
                    Ok(indexed) => indexed,
                    Err(err) => {
                        // The WRITE stands: the queue is derived state, so the row is safe and the
                        // worker will come back to it. Say so rather than claim a guarantee we missed.
                        tracing::warn!(key = %out.key, error = %err, "?wait=true could not index");
                        result["encode_error"] = serde_json::json!(err);
                        false
                    }
                });
                result["indexed"] = serde_json::json!(indexed);
            }
            tracing::info!(area = scope.area, key = %key, "memory stored over http");
            let text = match indexed {
                Some(indexed) => {
                    format!("stored {key} (queued={}, indexed={indexed})", out.queued)
                }
                None => format!("stored {key} (queued={})", out.queued),
            };
            render(StatusCode::OK, json, json_ok(result), text)
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
    // The bearer proves the DOOR; the identity pair names the PEN. A removal with no proven pen has
    // nobody to name in the ledger, and `store::delete` will refuse it on a row somebody owns.
    let pen = crate::identity::resolve(&headers).ok().flatten();
    let lookup = key.clone();
    match server
        .with_store("memory_delete", move |s| {
            if !s.rm(&lookup, pen.as_deref())? {
                return Ok(None);
            }
            Ok(Some(s.version_count(&lookup)?))
        })
        .await
    {
        Ok(Some(remaining)) => {
            tracing::info!(area = scope.area, key = %key, versions = remaining, "memory removed over http");
            render(
                StatusCode::OK,
                json,
                json_ok(serde_json::json!({
                    "status": "removed",
                    "key": key,
                    // The newest of these is the state it was deleted in (the delete snapshots it).
                    "versions_remaining": remaining,
                })),
                format!(
                    "deleted {key} ({remaining} version(s) remain, the newest being the state it had)"
                ),
            )
        }
        Ok(None) => error(
            json,
            StatusCode::NOT_FOUND,
            format!("no such memory: {key}"),
        ),
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /memory/{key}/upvote` — mark one memory useful (`memory_upvote`).
pub async fn upvote(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    // MCP's `memory_upvote` takes `{"keys": [...]}` — a BATCH, and that is what a caller who knows the
    // tool will send. Accept it; the path key is the one-key shorthand. A body that names other keys
    // must never be ignored, and this route used to upvote the PATH key and silently drop the body.
    let body = match json_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let keys = match body.as_ref().and_then(|b| b.get("keys")) {
        Some(serde_json::Value::Array(items)) => {
            let mut keys = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(k) => keys.push(k.to_string()),
                    None => {
                        return error(
                            json,
                            StatusCode::BAD_REQUEST,
                            format!("bad `keys` entry: {item} (a string expected)"),
                        );
                    }
                }
            }
            keys
        }
        Some(other) => {
            return error(
                json,
                StatusCode::BAD_REQUEST,
                format!("bad `keys`: {other} (an array of keys expected)"),
            );
        }
        None => vec![key.clone()],
    };
    let named = keys.join(", ");
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    // One implementation, two doors: the tool and this route share `upvote_sweep`, so neither the
    // errors nor the JSON can drift apart.
    match crate::mcp::upvote_sweep(server, reader_from(&headers), keys).await {
        Ok(payload) => {
            tracing::info!(area = scope.area, keys = %named, "upvoted over http");
            render(StatusCode::OK, json, payload, format!("upvoted {named}"))
        }
        Err(message) => {
            let status = error_status(&message);
            error(json, status, message)
        }
    }
}

/// `POST /memory/{key}/merge?into=` — integrate one memory into another and delete it (`memory_merge`).
pub async fn merge(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    // The MCP tool takes `{"from", "into"}`; accept it, so a caller can send the tool's own arguments
    // unchanged. The path names `from` here, so a body `from` must AGREE with it rather than quietly
    // win. `?into=` stays for a caller that only has a URL.
    let body = match json_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    if let Some(given) = body
        .as_ref()
        .and_then(|b| b.get("from"))
        .and_then(|f| f.as_str())
        && given != key
    {
        return error(
            json,
            StatusCode::BAD_REQUEST,
            format!("body `from` is `{given}` but the path names `{key}`"),
        );
    }
    let into = match body
        .as_ref()
        .and_then(|b| b.get("into"))
        .and_then(|i| i.as_str())
    {
        Some(into) => into.to_string(),
        None => match params.maybe("into") {
            Some(into) => into.to_string(),
            None => {
                return error(
                    json,
                    StatusCode::BAD_REQUEST,
                    "missing `into`: send `?into=<key>`, or a JSON body {\"into\": \"<key>\"}",
                );
            }
        },
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
                format!(
                    "merged {key} -> {into} ({} line(s) integrated, {} refs repointed)",
                    out.integrated.len(),
                    out.bodies
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /reparse` — rebuild the `[[link]]` graph from every body (`memory_reparse`).
///
/// Model-free and seconds long, which is the whole point: the alternative was a full re-embed.
pub async fn reparse(
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
        .with_store("memory_reparse", move |s| s.reparse())
        .await
    {
        Ok(report) => {
            tracing::info!(
                area = scope.area,
                rows = report.rows,
                edges = report.edges,
                "re-parsed the link graph over http"
            );
            render(
                StatusCode::OK,
                json,
                json_ok(serde_json::json!({
                    "status": "reparsed",
                    "rows": report.rows,
                    "edges": report.edges,
                })),
                format!(
                    "re-parsed {} document(s); the graph now holds {} edge(s)",
                    report.rows, report.edges
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /reembed` — queue every document that needs (re-)embedding (`memory_reembed`).
///
/// It used to be the heaviest lock holder in the store: one transaction from the first row to the last,
/// ~45 minutes on a 291-row store, with every write timing out for the duration. Now it enqueues and
/// returns, so it is one of the cheapest.
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
                queued = report.queued,
                "queued a re-embed over http"
            );
            render(
                StatusCode::OK,
                json,
                json_ok(serde_json::json!({
                    "status": "queued",
                    "queued": report.queued,
                    "dim": report.dim,
                    "rebuilt": report.rebuilt,
                    "model": report.model,
                })),
                format!(
                    "queued {} document(s) for re-embedding ({} rebuilt)",
                    report.queued, report.rebuilt
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
        "chars" => Some(FieldParam::Chars),
        "distance" => Some(FieldParam::Distance),
        _ => None,
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

/// The `GET /memory/{key}` prose: the header fields, then the body.
fn memory_text(mem: &store::Memory, relation: Option<&str>) -> String {
    let mut out = format!("{}\n", mem.key);
    out.push_str(&format!("  {:<8}{}\n", "title", mem.title));
    // Who answers for it, and what they are to the reader: a name is an address, and the walk is the
    // rest of the sentence.
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

/// How much of an index entry's title the boot keeps. The index exists so a cold client knows what it
/// HAS, not what it says — the body is one call away — and the whole document must survive being
/// fetched by a client that truncates on size (see [`BOOT_BUDGET_BYTES`]).
const INDEX_TITLE_CHARS: usize = 40;

/// How much of a project's goal the boot keeps, for the same reason.
const PROJECT_GOAL_CHARS: usize = 56;

/// The ceiling the default boot is held under, in bytes — public because the suite asserts it
/// (`tests/http.rs`), and a budget nothing checks is a comment.
///
/// Not a style preference: a fetching client truncates a large response, and a boot truncated
/// mid-way is worse than no boot — the reader gets a document it believes is complete. The default
/// view is the resident set plus the calls that reach everything else, so it stays small; the 14-tool
/// catalogue (every argument schema) is `?tools=true` and says so, because it is the one part that
/// cannot be made small and must never be silently cut in half.
pub const BOOT_BUDGET_BYTES: usize = 8_192;

/// What `/bootstrap` found when it looked. Every field is `None`/empty when the store could not be
/// read — the boot still says what it is and what it can be asked for, and names the missing facts
/// rather than omitting them, because silence reads as "nothing to report".
#[derive(Default)]
struct BootView {
    /// The resident note: `(key, title, body)`.
    note: Option<(String, String, String)>,
    /// The self set's tag for THIS caller — `self:<pen>`; empty when no pen was proven.
    tag: String,
    /// The self set as `(key, title)`, key-ascending.
    self_set: Vec<(String, String)>,
    /// Open projects as `(key, status, goal)`.
    projects: Vec<(String, String, String)>,
    /// The one-line state, or `None` when there was nothing to read.
    state: Option<String>,
    /// Why there is no content, when there is none.
    unreadable: Option<String>,
}

impl BootView {
    /// Read the boot from the store on a read-only connection of our own.
    fn read(paths: &Paths, reader: &crate::store::audience::Reader) -> Self {
        let conn = match db::open_read_only(paths) {
            Ok(conn) => conn,
            Err(err) => {
                return BootView {
                    unreadable: Some(err.to_string()),
                    ..BootView::default()
                };
            }
        };
        match Self::fill(&conn, reader) {
            Ok(view) => view,
            Err(err) => BootView {
                unreadable: Some(err.to_string()),
                ..BootView::default()
            },
        }
    }

    fn fill(
        conn: &rusqlite::Connection,
        reader: &crate::store::audience::Reader,
    ) -> crate::error::Result<Self> {
        let pen = reader.pen_name();
        let note = boot_candidates(pen)
            .into_iter()
            .find_map(|key| {
                conn.query_row(
                    "SELECT key, title, body FROM memory WHERE key = ?1",
                    rusqlite::params![key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .transpose()
            })
            .transpose()?;
        let tag = boot_tag(pen).unwrap_or_default();
        let self_set = match boot_tag(pen) {
            Some(tag) => tags::keys_with_seen(conn, &tag, reader)?,
            None => Vec::new(),
        };
        Ok(BootView {
            note,
            tag,
            self_set,
            projects: project::open(conn)?,
            state: Some(state_line(conn)?),
            unreadable: None,
        })
    }
}

/// The state line: the facts a caller needs before trusting anything else, all from the boot's own
/// read-only connection. A *view* of the same facts `/health` reports — no readiness predicate is
/// reimplemented here (readiness is `/health`'s job), and the encode queue is the metric it is there.
fn state_line(conn: &rusqlite::Connection) -> crate::error::Result<String> {
    let memories: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
    let projects = project::count(conn)?;
    let schema = db::migrate::version(conn)?;
    let latest = db::migrate::latest();
    let queue = encode::counts(conn)?;
    let status = if schema == latest {
        "ready"
    } else {
        "NOT READY"
    };
    let queue = match queue.oldest_s {
        Some(age) => format!(
            "{} pending (oldest {age}s), {} failed",
            queue.pending, queue.failed
        ),
        None => format!("{} pending, {} failed", queue.pending, queue.failed),
    };
    Ok(format!(
        "{status} — {memories} memories, {projects} projects, schema v{schema}, encode {queue} (detail: /health)"
    ))
}

/// The shared floor: the one document every caller may be given, whoever it is.
const BOOT_FLOOR: &str = "bootstrap";

/// The keys a caller's resident note is looked for under, in order.
///
/// ★ A DOOR THAT CANNOT NAME ITS CALLER MUST NOT NAME A CALLER (2026-10-22). This endpoint used to
/// inline `stet-self` for everybody and index the tag `cline-self` — one agent's first-person
/// identity, served to all — which is how a cold Cline came to narrate itself as Stet. The address is
/// now built from the PROVEN pen: `bootstrap-<pen>` when it exists, else the shared floor. Anonymous
/// callers get the floor and no self set, which is honest rather than generous.
fn boot_candidates(pen: Option<&str>) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(pen) = pen {
        keys.push(format!("bootstrap-{pen}"));
    }
    keys.push(BOOT_FLOOR.to_string());
    keys
}

/// The self set's tag for a caller: `self:<pen>`. `None` for a caller who proved nothing.
fn boot_tag(pen: Option<&str>) -> Option<String> {
    pen.map(|pen| format!("self:{pen}"))
}

/// `GET /bootstrap` — **the door, and what is behind it**: everything a cold client needs in one
/// read, in the order it needs it, small enough to arrive intact.
///
/// This is the view HTTP.md promised and the code never delivered. What it used to be was a verbatim
/// dump of `initialize` + all 14 tool cards — 14.8 KB, which a fetching client middle-truncated
/// (7 KB punched out of the tool list), and which contains no memory at all. A client that read it
/// learned the *shape of the API* and nothing about the store: not that there are 302 memories, not
/// that one of them is its own boot note, not that eight projects are open and waiting. So it did
/// not think of this endpoint as how you get memories — and it was right, because it was not.
///
/// So the boot leads with what is here and how to fetch it: the four calls that reach the store, the
/// resident note in full, an index of the self set, the work still open, and the style. The tool
/// catalogue is `?tools=true` — the one part that cannot be small, which the caller should choose to
/// spend tokens on rather than receive by default.
pub async fn bootstrap(
    State(state): State<HttpState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let with_tools = params.flag("tools");
    // The boot reads the store on its OWN read-only connection: no lock, so a boot never queues
    // behind a writer, and a boot during an outage still describes a store it cannot read.
    let view = tokio::task::spawn_blocking({
        let paths = state.paths.clone();
        move || BootView::read(&paths, &reader)
    })
    .await
    .unwrap_or_default();
    let tools = with_tools.then(boot_tools);
    render(
        StatusCode::OK,
        json,
        boot_json(&view, tools.as_deref()).to_string(),
        boot_text(&view, tools.as_deref()),
    )
}

/// The boot document in the store's text form: for a client to read, in the order it should read it.
///
/// The order is the point. A client arriving cold asks, in sequence: *what is this*, *what is in it*,
/// *how do I fetch it*, *what is mine*, *what is open*, *how does this person want me to write*,
/// *where is the rest*. The document answers in that order, and every line that names a memory or a
/// project is a call the reader can make.
fn boot_text(view: &BootView, tools: Option<&[BootTool]>) -> String {
    let mut out = String::from("# ellmephant — your memory, over plain HTTP\n");
    match &view.state {
        Some(state) => out.push_str(&format!("# {state}\n")),
        // A boot with no store still says what it is and what it can be asked for; the missing facts
        // are NAMED, because silence would read as "there is nothing in here".
        None => out.push_str(&format!(
            "# STORE UNREADABLE, API ONLY — {}\n",
            view.unreadable.as_deref().unwrap_or("unknown error")
        )),
    }

    out.push_str(
        "\nHOW TO GET AT IT — this endpoint is the door, not a handshake. Four calls reach everything:\n\
         \x20 GET /stats?format=json                 the whole store in one object (counts, tags, queue, top)\n\
         \x20 GET /memory/{key}?format=json          one memory whole, by key\n\
         \x20 GET /search?q=…&format=json            hunt by meaning; &tags=todo for what I meant to do next\n\
         \x20 POST /memory                          write: a JSON body of `memory_upsert`'s args (see ?tools=true)\n\
         \x20 Every GET takes ?format=json. Reads are open. If a write 401s, it wants a bearer — ask me.\n",
    );

    out.push_str(&format!(
        "\n## 1. YOU — resident, in full (memory `{}`)\n",
        view.note
            .as_ref()
            .map_or(BOOT_FLOOR, |(key, _, _)| key.as_str())
    ));
    match &view.note {
        Some((_, _, body)) => out.push_str(&format!("{}\n", indent_block(body, "  "))),
        None => out.push_str("  (no resident note in this store)\n"),
    }

    if view.tag.is_empty() {
        out.push_str(
            "\n## 2. THE REST OF YOU — no self set: this caller proved no pen\n\
             \x20 (a proved pen gets `self:<pen>` indexed here; anonymous callers get the floor only)\n",
        );
    } else {
        out.push_str(&format!(
            "\n## 2. THE REST OF YOU — tag `{}`, {} keys; a body is GET /memory/{{key}}\n",
            view.tag,
            view.self_set.len()
        ));
    }
    for (key, title) in &view.self_set {
        out.push_str(&format!("  {key:<34} {}\n", clip(title, INDEX_TITLE_CHARS)));
    }

    out.push_str(&format!(
        "\n## 3. OPEN WORK — {} projects still running; state, goal and `preload`: GET /project/{{key}}\n",
        view.projects.len()
    ));
    for (key, status, goal) in &view.projects {
        out.push_str(&format!(
            "  {status:<7} {key:<20} {}\n",
            clip(goal, PROJECT_GOAL_CHARS)
        ));
    }

    out.push_str(
        "\n## 4. STYLE — how to work here (the full standing text rides ?tools=true)\n\
         \x20 smart-caveman: big words in, small words out. Upvote any memory that shaped your task.\n\
         \x20 A project is an activity over time; a memory JOINS one by carrying the `prj:<key>` tag and\n\
         \x20 POINTS at one with `[[prj:key]]`. Keys are plain names, and the tool descriptions are the spec.\n",
    );

    match tools {
        Some(tools) => {
            out.push_str(&format!(
                "\n## 5. TOOLS — {}; HTTP verbs in place of MCP verbs, arguments and descriptions unchanged\n",
                tools.len()
            ));
            out.push_str(&catalogue_text(tools));
        }
        None => {
            // The size is MEASURED, not stated: it is the one number a caller deciding whether to
            // spend the tokens needs, and a hardcoded size drifts the first time a tool description
            // grows. Rendering it here costs nothing (the catalogue is code, not a database read).
            let catalogue = catalogue_text(&boot_tools());
            out.push_str(&format!(
                "\n## 5. THE FULL CATALOGUE — all {} tools with every argument: GET /bootstrap?tools=true\n\
                 \x20 That document is {} KB of schemas — several times this one — and some fetching clients\n\
                 \x20 truncate a response that size, so it is opt-in. Everything above is the common case;\n\
                 \x20 that is the reference. Nothing in it is needed to fetch a memory.\n",
                boot_tools().len(),
                catalogue.len() / 1024
            ));
        }
    }
    out
}

/// The catalogue half of the boot: every tool, its verb, and its arguments. Split out so the default
/// view can measure it without including it.
fn catalogue_text(tools: &[BootTool]) -> String {
    let mut out = String::new();
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

/// Cut `text` to at most `max` characters on a word boundary, marking the cut — an index entry says
/// what a memory is ABOUT, and a hard cut mid-word reads as corruption where a marked cut reads as a
/// limit. Every index line in the boot goes through this.
fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let cut = head
        .rfind(' ')
        .map(|i| head[..i].to_string())
        .unwrap_or(head);
    format!("{cut} …")
}

/// The boot document as `?format=json` — the MCP handshake's own shape.
///
/// The fields are `initialize`'s (`serverInfo`, `capabilities`, `instructions`) plus `tools/list`'s
/// (`tools`), and each tool carries `method`/`path` **where MCP carried `name`**. Everything else on
/// a card — `title`, `description`, `inputSchema`, `annotations` — is the MCP field untouched.
///
/// `protocolVersion` is the one field an MCP boot has that this does not: it names a *transport*
/// version, and over HTTP there is no MCP protocol to negotiate. Left out rather than faked.
/// The boot document as `?format=json` — the same facts, shaped for a machine.
///
/// `serverInfo`/`capabilities`/`instructions` stay in MCP's own names so a client moving over from
/// MCP can read what it already knows how to read; `tools` appears only with `?tools=true`, and the
/// content sections carry the same cuts as the text form so the two cannot diverge in size or shape.
fn boot_json(view: &BootView, tools: Option<&[BootTool]>) -> serde_json::Value {
    let mut doc = serde_json::json!({
        "serverInfo": { "name": SERVER_NAME, "version": crate::VERSION },
        "capabilities": { "tools": {} },
        "instructions": instructions(),
        "howToUse": {
            "what": "This is a memory store reachable over plain HTTP. It is the door, not a handshake.",
            "calls": [
                { "call": "GET /stats?format=json", "for": "the whole store in one object (counts, tags, encode queue, top tags/useful)" },
                { "call": "GET /memory/{key}?format=json", "for": "one memory whole, by key" },
                { "call": "GET /search?q=…&format=json", "for": "hunt by meaning; &tags=todo for what I meant to do next" },
                { "call": "GET /search?format=json&tags=cline-self", "for": "every key of the resident self set" },
                { "call": "GET /projects?format=json", "for": "all projects; ?status=active to narrow" },
                { "call": "POST /memory", "for": "write: a JSON body of `memory_upsert`'s args" },
                { "call": "GET /bootstrap?tools=true", "for": "the full tool catalogue with every argument (≈15KB)" }
            ],
            "notes": [
                "Every GET takes ?format=json; reads are open.",
                "A write may 401: that means it wants a bearer token.",
                "A write returns before its vector exists (the store embeds it a moment later)."
            ]
        },
        "state": view.state,
        "unreadable": view.unreadable,
        "youResident": view.note.as_ref().map(|(key, title, body)| serde_json::json!({
            "key": key, "title": title, "body": body,
        })),
        "selfSet": {
            "tag": view.tag,
            "keys": view.self_set.iter().map(|(key, title)| serde_json::json!({
                "key": key, "title": clip(title, INDEX_TITLE_CHARS),
            })).collect::<Vec<_>>(),
        },
        "openProjects": view.projects.iter().map(|(key, status, goal)| serde_json::json!({
            "key": key, "status": status, "goal": clip(goal, PROJECT_GOAL_CHARS),
            "call": format!("GET /project/{key}?hydrate=full"),
        })).collect::<Vec<_>>(),
    });
    if let Some(tools) = tools {
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
        doc["tools"] = serde_json::Value::Array(tools);
    }
    doc
}

/// `POST /mail` — write one letter (`mail_send`). JSON body; nothing is delivered.
pub async fn mail_send(
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
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        Ok(None) => return error(json, StatusCode::BAD_REQUEST, "a JSON body is required"),
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let field = |k: &str| doc.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let (Some(sender), Some(recipient), Some(letter)) =
        (field("sender"), field("recipient"), field("body"))
    else {
        return error(
            json,
            StatusCode::BAD_REQUEST,
            "`sender`, `recipient` and `body` are required",
        );
    };
    let subject = field("subject").unwrap_or_default();
    let encoding = field("encoding").unwrap_or_else(|| "text".to_string());
    let to = recipient.clone();
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("mail_send", move |s| {
            s.mail_send(&sender, &recipient, &subject, &letter, &encoding)
        })
        .await
    {
        Ok(out) => {
            tracing::info!(area = scope.area, seq = out.seq, to = %to, "letter over http");
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&out).unwrap_or_default(),
                format!("letter {} written to `{to}`", out.seq),
            )
        }
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// `GET /mail?recipient=&since=&limit=` — envelopes after the reader's cursor (`mail_inbox`).
///
/// ★ THE CURSOR BELONGS TO THE PROVEN PEN (migration 0017). With `X-Stet-From`/`X-Stet-Auth` the
/// request reads that pen's stored cursor; an explicit `since` still wins, because a diagnostic may
/// want to look again. Anonymous is NOT an error here — reads are open — it simply has no stored
/// cursor, so `since` (or 0) decides, and nothing is written.
/// The proven pen behind an HTTP request, or the refusal to send.
#[allow(clippy::result_large_err)] // the refusal IS the axum response; boxing it would be taste, not safety.
fn mail_reader_identity(headers: &HeaderMap, json: bool) -> Result<String, Response> {
    match crate::identity::resolve(headers) {
        Ok(Some(name)) => Ok(name),
        Ok(None) => Err(error(
            json,
            StatusCode::FORBIDDEN,
            "refused: reading mail needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`",
        )),
        Err(forged) => Err(error(json, StatusCode::FORBIDDEN, forged.to_string())),
    }
}

/// ★ THE BOX IS THE BOUNDARY, on every door (soma/ellmephant#92).
///
/// The MCP tool has refused a non-recipient since 2026-10-18; this surface resolved an identity only
/// to find the reader's CURSOR and then served any box to anyone. One rule, one place, both doors —
/// `mail box?recipient=stet` answered 49 envelopes to an anonymous caller, measured 2026-10-22.
#[allow(clippy::result_large_err)] // same: a refusal is returned whole, as on every other handler here.
fn mail_guard(reader: &str, requested: &str, json: bool) -> Result<(), Response> {
    if crate::mcp::may_read_mail(reader, requested, &crate::mcp::surrogates_from_env()) {
        Ok(())
    } else {
        Err(error(
            json,
            StatusCode::FORBIDDEN,
            format!(
                "refused: `{reader}` may not read `{requested}`'s mail — only the recipient, or a \
                 declared surrogate, may"
            ),
        ))
    }
}

pub async fn mail_inbox(
    State(state): State<HttpState>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let Some(recipient) = params.maybe("recipient").map(str::to_string) else {
        return error(json, StatusCode::BAD_REQUEST, "`recipient` is required");
    };
    let explicit = params.maybe("since").and_then(|v| v.parse::<i64>().ok());
    let limit = params
        .maybe("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_usize);
    let reader = match mail_reader_identity(&headers, json) {
        Ok(name) => name,
        Err(r) => return r,
    };
    if let Err(r) = mail_guard(&reader, &recipient, json) {
        return r;
    }
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let me = Some(reader.clone());
    match server
        .with_read(
            "mail_inbox",
            crate::store::audience::Reader::everything(),
            move |s| {
                // The box's top, the page's top, and the READER's place are three facts (see the MCP tool).
                let stored = match &me {
                    Some(name) => s.mail_cursor(name, &recipient)?,
                    None => None,
                };
                let since = explicit.unwrap_or_else(|| stored.as_ref().map(|c| c.seq).unwrap_or(0));
                Ok((
                    s.mail_inbox(&recipient, since, limit)?,
                    s.mail_head(&recipient)?,
                    stored,
                    since,
                ))
            },
        )
        .await
    {
        Ok((rows, head, stored, since)) => {
            let page_top = rows.iter().map(|r| r.seq).max();
            let cursor = stored.as_ref().map(|c| c.seq).unwrap_or(0);
            let text = if rows.is_empty() {
                format!("no mail after {since}")
            } else {
                rows.iter()
                    .map(|r| {
                        format!(
                            "{} {} {} ({} · {} bytes)",
                            r.seq, r.at, r.sender, r.subject, r.bytes
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            render(
                StatusCode::OK,
                json,
                serde_json::json!({
                    "reader": stored
                        .as_ref()
                        .map(|c| c.reader.clone())
                        .unwrap_or_else(|| reader.clone()),
                    "cursor": cursor,
                    "head": head,
                    "unread": (head - cursor).max(0),
                    "letters": rows,
                    "page_top": page_top,
                    "max_seq": page_top.unwrap_or(since),
                    "newest_available": head,
                })
                .to_string(),
                text,
            )
        }
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// `POST /mail/ack?recipient=&seq=` — advance the caller's own cursor (`mail_ack`).
///
/// ★ A CURSOR NEEDS A READER, so this is the one mail route that REFUSES anonymous: there is no row
/// to move without a proven pen, and guessing one would hand someone else's cursor to whoever asked.
pub async fn mail_ack(
    State(state): State<HttpState>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let (Some(recipient), Some(seq)) = (
        params.maybe("recipient").map(str::to_string),
        params.maybe("seq").and_then(|v| v.parse::<i64>().ok()),
    ) else {
        return error(
            json,
            StatusCode::BAD_REQUEST,
            "`recipient` and `seq` are required",
        );
    };
    let reader = match crate::identity::resolve(&headers) {
        Ok(Some(name)) => name,
        Ok(None) => {
            return error(
                json,
                StatusCode::FORBIDDEN,
                "refused: a cursor belongs to a proven pen — send `X-Stet-From` and `X-Stet-Auth`",
            );
        }
        Err(forged) => return error(json, StatusCode::FORBIDDEN, forged.to_string()),
    };
    if let Err(r) = mail_guard(&reader, &recipient, json) {
        return r;
    }
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("mail_ack", move |s| {
            s.mail_advance(&reader, &recipient, seq)
        })
        .await
    {
        Ok(cursor) => render(
            StatusCode::OK,
            json,
            serde_json::to_string(&cursor).unwrap_or_default(),
            format!(
                "cursor moved to {} (+{}), {} unread",
                cursor.seq, cursor.moved, cursor.unread
            ),
        ),
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /mail/{seq}` — one letter, body included (`mail_read`).
pub async fn mail_read(
    State(state): State<HttpState>,
    axum::extract::Path(seq): axum::extract::Path<i64>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let reader = match mail_reader_identity(&headers, json) {
        Ok(name) => name,
        Err(r) => return r,
    };
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_read(
            "mail_read",
            crate::store::audience::Reader::everything(),
            move |s| s.mail_read(seq),
        )
        .await
    {
        Ok(Some(letter)) => {
            // The LETTER decides its box: the check is on the row, not on an argument.
            if let Err(r) = mail_guard(&reader, &letter.recipient, json) {
                return r;
            }
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&letter).unwrap_or_default(),
                letter.body.clone(),
            )
        }
        Ok(None) => error(json, StatusCode::NOT_FOUND, "no such letter"),
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// `POST /mail/prune?keep_days=7` — drop letters nobody ever read (`mail_prune`).
pub async fn mail_prune(
    State(state): State<HttpState>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let keep_days = params
        .maybe("keep_days")
        .and_then(|v| v.parse().ok())
        .unwrap_or(14_i64);
    let server = match session(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match server
        .with_store("mail_prune", move |s| s.mail_prune(keep_days))
        .await
    {
        Ok(removed) => render(
            StatusCode::OK,
            json,
            serde_json::json!({ "removed": removed }).to_string(),
            format!("pruned {removed} letter(s) older than {keep_days}d"),
        ),
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}
