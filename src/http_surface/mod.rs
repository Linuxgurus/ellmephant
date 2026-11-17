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
    if message.starts_with("forbidden:") {
        StatusCode::FORBIDDEN
    } else if message.starts_with("no such memory:") || message.starts_with("no such project:") {
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
/// ★ THE SESSION IS AN EXTRACTOR, NOT SIXTY-TWO PREAMBLES (soma/ellmephant#233).
///
/// Every handler in this file began with
///
/// ```ignore
/// let server = match session(&state) { Ok(s) => s, Err(r) => return r };
/// ```
///
/// which is the same sentence sixty-two times: "if the router forgot to attach the session, answer
/// 500". It is a property of the request, so it belongs in the request's type. `Server` is a thin
/// borrow of the `MemoryServer` — it derefs, so a handler's body is unchanged — and its rejection is
/// the same 500 the preamble built.
pub struct Server(MemoryServer);

impl std::ops::Deref for Server {
    type Target = MemoryServer;

    fn deref(&self) -> &MemoryServer {
        &self.0
    }
}

impl axum::extract::FromRequestParts<HttpState> for Server {
    type Rejection = Response;

    async fn from_request_parts(
        _parts: &mut axum::http::request::Parts,
        state: &HttpState,
    ) -> std::result::Result<Self, Self::Rejection> {
        state.server().cloned().map(Server).ok_or_else(|| {
            error(
                false,
                StatusCode::INTERNAL_SERVER_ERROR,
                "server not attached",
            )
        })
    }
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

    /// A boolean flag (`true` or `1`; everything else, including absent, is `false`).
    fn flag(&self, name: &str) -> bool {
        matches!(self.maybe(name), Some("true") | Some("1"))
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
    audience: Option<String>,
    /// `Force: true` — write a run shape here anyway (soma/ellmephant#221). Consumed by the header
    /// parse, so it never reaches the body: a leaked `Force: true` would live in every future diff.
    force: Option<bool>,
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
            "Audience" => out.audience = Some(value.to_string()),
            "Force" => {
                out.force = Some(match value.to_ascii_lowercase().as_str() {
                    "true" | "yes" | "1" => true,
                    "false" | "no" | "0" | "" => false,
                    other => return Err(format!("`Force: {other}` is not true|false")),
                })
            }
            "Tags" => {
                out.tags = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(ToString::to_string)
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
            acted_by: None,
            key: Some(key),
            title: fm.title,
            body: Some(fm.body),
            source: fm.source,
            // A text body carries no speaker: the handler stamps `asserted_by` from the request.
            asserted_by: None,
            origin: fm.origin,
            tags: fm.tags,
            private: fm.private,
            audience: fm.audience,
            version_note: fm.version_note,
            expected_version: None,
            // ★ THE TEXT DOOR HAS THE OVERRIDE TOO (Stet, review of #221). Without it the only way
            // past the gate here is to EDIT THE PROSE until it stops looking like a receipt, which
            // corrupts the record to satisfy a linter — worse than the log we are stopping.
            force: fm.force.unwrap_or(false),
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
    /// An explicit audience — a SHARE: `public`, or `pen:<name>` / `subtree:<name>` addresses
    /// (comma-separated, at most 8). Validated against the registry; unlike `private`, it names others
    /// on purpose. Omit to keep the stored audience.
    audience: Option<String>,
    /// The CAS token, as `memory_get` returns it.
    expected_version: Option<i64>,
    /// Bypass the run-shape gate (soma/ellmephant#217).
    force: Option<bool>,
}

/// A `memory_upsert` patch from a JSON body.
fn memory_patch_json(bytes: &[u8]) -> Result<(store::Patch, Option<String>), String> {
    let m: MemoryJson = serde_json::from_slice(bytes).map_err(|e| format!("bad JSON: {e}"))?;
    let as_pen = m.as_pen;
    Ok((
        store::Patch {
            acted_by: None,
            key: m.key,
            title: m.title,
            body: m.body,
            source: m.source,
            asserted_by: None,
            origin: m.origin,
            tags: m.tags,
            private: m.private,
            audience: m.audience,
            version_note: m.version_note,
            expected_version: m.expected_version,
            force: m.force.unwrap_or(false),
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

/// ★ UNAVAILABLE IS NOT FORGED (2026-10-30, Stet on #204): the split exists in `identity::Denied`
/// because a store that cannot be read is not a caller who lied. This is where it reaches the
/// client — 503 for the store, the route's own refusal code for a bad proof. Rotating secrets
/// because a database blinked is the failure this prevents.
fn denied_response(json: bool, denied: crate::identity::Denied, forged: StatusCode) -> Response {
    let code = match denied {
        crate::identity::Denied::Forged(_) => forged,
        crate::identity::Denied::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    error(json, code, denied.to_string())
}

/// A [`Caller`](crate::identity::Caller) refusal as a response: the denial keeps its own code split
/// (503 for an unreadable store, the route's code for a bad proof), and an absent pen gets the
/// route's sentence. One mapper, so no door invents a fourth answer.
fn refusal_response(json: bool, refusal: crate::identity::Refusal, absent: StatusCode) -> Response {
    match refusal {
        crate::identity::Refusal::Denied(denied) => denied_response(json, denied, absent),
        crate::identity::Refusal::Missing(why) => error(json, absent, why),
    }
}

/// The proven pen of a request, or the 401 that says what to send — the sudo routes all need one,
/// because authority is always borrowed BY somebody.
#[allow(clippy::result_large_err)] // the Err is a ready-to-return response, as in `session`
fn proven_pen(headers: &HeaderMap, json: bool, paths: &Paths) -> Result<String, Response> {
    crate::identity::Caller::from_headers(paths, headers)
        .pen_or("refused: this needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`")
        .map(ToString::to_string)
        .map_err(|r| refusal_response(json, r, StatusCode::UNAUTHORIZED))
}

/// One required string field of a JSON body, or the 400 that names what is missing.
#[allow(clippy::result_large_err)] // the Err is a ready-to-return response, as elsewhere here
fn body_str(doc: &serde_json::Value, name: &str, json: bool) -> Result<String, Response> {
    doc.get(name)
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .ok_or_else(|| error(json, StatusCode::BAD_REQUEST, format!("missing `{name}`")))
}

/// Who this request reads as. A proven pair decides; anything else — including a forged claim — is
/// nobody, and nobody sees public rows only. Reads never fail for want of a pen.
fn reader_from(headers: &HeaderMap, paths: &Paths) -> crate::store::audience::Reader {
    crate::identity::Caller::from_headers(paths, headers).reader()
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
        query: p.maybe("q").map(ToString::to_string),
        pattern: p.maybe("pattern").map(ToString::to_string),
        mode: Some(match_mode_param(p.maybe("mode"))?),
        k: p.parse_as::<usize>("k")?,
        max_distance: p.parse_as::<f32>("max_distance")?,
        // The MCP door has `semantic_only`; the HTTP door must not be the one place it is
        // unreachable. Absent stays `None`, so today's callers see no change.
        semantic_only: p
            .maybe("semantic_only")
            .map(|v| bool_param(v, "semantic_only"))
            .transpose()?,
        expand: p.parse_as::<u8>("expand")?,
        tags,
        min_chars: p.parse_as::<usize>("min_chars")?,
        max_chars: p.parse_as::<usize>("max_chars")?,
        created_after: p.maybe("created_after").map(ToString::to_string),
        created_before: p.maybe("created_before").map(ToString::to_string),
        updated_after: p.maybe("updated_after").map(ToString::to_string),
        updated_before: p.maybe("updated_before").map(ToString::to_string),
        // ★ THE SAME PREDICATES ON BOTH DOORS (soma/ellmephant#114): a filter reachable from one
        // door and not the other is a search that answers differently depending on how it was asked.
        key: p.maybe("key").map(ToString::to_string),
        key_prefix: p
            .maybe("key_prefix")
            .map(|v| bool_param(v, "key_prefix"))
            .transpose()?,
        title: p.maybe("title").map(ToString::to_string),
        status: p.maybe("status").map(ToString::to_string),
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

/// A query-string boolean: `1`/`true`/`yes` and `0`/`false`/`no`; anything else is a clean
/// error. A silently-false `semantic_only=1` would answer a different question than the caller
/// asked, which is the failure mode #215 exists to expose.
fn bool_param(raw: &str, name: &str) -> Result<bool, String> {
    match raw.trim() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        other => Err(format!(
            "bad `{name}`: {other} (want 1/true/yes or 0/false/no)"
        )),
    }
}

/// One hit field name, or `None` if it is not a field.
fn field_param(name: &str) -> Option<FieldParam> {
    match name {
        "key" => Some(FieldParam::Key),
        "via" => Some(FieldParam::Via),
        "title" => Some(FieldParam::Title),
        "tags" => Some(FieldParam::Tags),
        "chars" => Some(FieldParam::Chars),
        "distance" => Some(FieldParam::Distance),
        "score" => Some(FieldParam::Score),
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
        // Two numbers, because they are two facts that share one value (owner NULL).
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

/// The ceiling the default boot is held under, in bytes — public because the suite asserts it
/// (`tests/http.rs`), and a budget nothing checks is a comment.
///
/// Not a style preference: a fetching client truncates a large response, and a boot truncated
/// mid-way is worse than no boot — the reader gets a document it believes is complete. The default
/// view is the resident set plus the calls that reach everything else, so it stays small; the 14-tool
/// catalogue (every argument schema) is `?tools=true` and says so, because it is the one part that
/// cannot be made small and must never be silently cut in half.
pub const BOOT_BUDGET_BYTES: usize = 8_192;

#[cfg(test)]
mod front_matter_tests {
    use super::front_matter;

    /// ★ THE OVERRIDE MUST NOT LEAK INTO THE DOCUMENT (Stet, review of #221): `Force:` is consumed by
    /// the header parse, so the versioned body is byte-identical with and without it.
    #[test]
    fn force_is_consumed_and_never_stored() {
        let text =
            "Key: a-run\nTitle: a run\nForce: true\n---\n14:41Z one\n14:42Z two\n14:43Z three";
        let fm = front_matter(text).expect("front matter");
        assert_eq!(fm.force, Some(true));
        assert!(
            !fm.body.contains("Force"),
            "the override must not reach the body: {}",
            fm.body
        );
        let plain =
            front_matter("Key: a-run\nTitle: a run\n---\n14:41Z one\n14:42Z two\n14:43Z three")
                .expect("front matter");
        assert_eq!(
            plain.body, fm.body,
            "same body with and without the override"
        );
    }

    #[test]
    fn a_bad_force_says_so() {
        let err = front_matter("Key: k\nForce: perhaps\n---\nbody").unwrap_err();
        assert!(err.contains("Force"), "{err}");
    }
}

#[cfg(test)]
mod api_tests {
    use super::*;

    #[test]
    fn the_index_carries_every_route_and_stays_lean() {
        let doc = api_doc(None, false).expect("index");
        let ops = doc["ops"].as_array().expect("ops");
        assert_eq!(ops.len(), ROUTES.len(), "every route, once");
        assert!(
            ops.iter().all(|op| op.get("schema").is_none()),
            "the default index must not carry schemas — that is the cost it exists to avoid"
        );
        assert!(
            ops.iter().any(|op| op["group"] == "hygiene"),
            "the hygiene group is the reason the index exists"
        );
        assert!(
            ops.iter().all(|op| op["core"].as_bool().is_some()),
            "every op says whether the core door already carries it"
        );
    }

    #[test]
    fn full_adds_schemas_and_a_group_narrows() {
        let full = api_doc(None, true).expect("full index");
        assert!(
            full["ops"]
                .as_array()
                .unwrap()
                .iter()
                .all(|op| op.get("schema").is_some())
        );
        let hygiene = api_doc(Some("hygiene"), false).expect("hygiene group");
        assert!(
            hygiene["ops"]
                .as_array()
                .unwrap()
                .iter()
                .all(|op| op["group"] == "hygiene")
        );
        assert!(api_doc(Some("no-such-group"), false).is_none());
    }
}

mod pen;
pub use pen::*;
mod memory;
pub use memory::*;
mod config;
pub use config::*;
mod claim;
pub use claim::*;
mod write;
pub use write::*;
mod boot;
pub use boot::*;
mod mail;
pub use mail::*;
mod channel;
pub use channel::*;
mod api;
pub use api::*;
mod routes;
pub use routes::*;
