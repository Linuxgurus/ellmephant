use axum::body::{Body, to_bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;

use crate::config::Paths;
use crate::db;
use crate::http::HttpState;
use crate::mcp::{
    DUPES_LIMIT, FieldParam, GET_NUDGE, HISTORY_LIMIT, MatchModeParam, MemoryServer, SERVER_NAME,
    SearchParams, SemanticSearchParams, instructions, json_ok, tool_catalog,
};
use crate::search::MatchMode;
use crate::store::{self, encode, project, tags};
use crate::wire::links::{backlinks_to_wire_json, dangling_to_wire_json};
use rusqlite::OptionalExtension;

#[derive(Clone, Copy)]
pub struct Scope {
    pub area: &'static str,
    pub namespace: Option<&'static str>,
}

const STORE_SCOPE: Scope = Scope {
    area: "store",
    namespace: None,
};

const MAX_BODY: usize = 4 * 1024 * 1024;

fn resolve(provided: Option<&str>, expected: Option<&str>) -> Option<Scope> {
    match (provided, expected) {
        (_, None) => Some(STORE_SCOPE),
        (Some(p), Some(e)) if p == e => Some(STORE_SCOPE),
        _ => None,
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

fn authorize(state: &HttpState, headers: &HeaderMap, json: bool) -> ApiResult<Scope> {
    resolve(bearer(headers), state.write_token()).ok_or_else(|| ApiError::unauthorized(json))
}

fn render(
    status: StatusCode,
    json: bool,
    json_body: String,
    text_body: String,
) -> ApiResult<Response> {
    let (content_type, body) = if json {
        ("application/json", json_body)
    } else {
        ("text/plain; charset=utf-8", text_body)
    };
    Ok(Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("http response"))
}

pub struct Server(MemoryServer);

impl std::ops::Deref for Server {
    type Target = MemoryServer;

    fn deref(&self) -> &MemoryServer {
        &self.0
    }
}

impl axum::extract::FromRequestParts<HttpState> for Server {
    type Rejection = ApiError;

    async fn from_request_parts(
        _parts: &mut axum::http::request::Parts,
        state: &HttpState,
    ) -> std::result::Result<Self, Self::Rejection> {
        state
            .server()
            .cloned()
            .map(Server)
            .ok_or_else(|| ApiError::internal(false, "server not attached"))
    }
}

struct Params(Vec<(String, String)>);

impl Params {
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

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn maybe(&self, name: &str) -> Option<&str> {
        self.get(name).map(str::trim).filter(|v| !v.is_empty())
    }

    fn json(&self) -> bool {
        self.get("format") == Some("json")
    }

    fn parse_as<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, String> {
        match self.maybe(name) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<T>()
                .map(Some)
                .map_err(|_| format!("bad `{name}`: {raw}")),
        }
    }

    fn flag(&self, name: &str) -> bool {
        matches!(self.maybe(name), Some("true") | Some("1"))
    }
}

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

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

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
    force: Option<bool>,
    body: String,
}

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
            asserted_by: None,
            origin: fm.origin,
            tags: fm.tags,
            private: fm.private,
            audience: fm.audience,
            version_note: fm.version_note,
            expected_version: None,
            force: fm.force.unwrap_or(false),
        },
        None,
    ))
}

#[derive(serde::Deserialize)]
struct MemoryJson {
    key: Option<String>,
    title: Option<String>,
    body: Option<String>,
    source: Option<String>,
    origin: Option<String>,
    tags: Option<Vec<String>>,
    version_note: Option<String>,
    #[serde(default, rename = "as")]
    as_pen: Option<String>,
    private: Option<bool>,
    audience: Option<String>,
    expected_version: Option<i64>,
    force: Option<bool>,
}

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

fn project_patch_json(bytes: &[u8]) -> Result<project::Patch, String> {
    let p: ProjectJson = serde_json::from_slice(bytes).map_err(|e| format!("bad JSON: {e}"))?;
    Ok(p.into_patch())
}

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

fn is_json_content(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("application/json"))
        .unwrap_or(false)
}

async fn body_bytes(body: Body) -> Result<Vec<u8>, String> {
    to_bytes(body, MAX_BODY)
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("bad body: {e}"))
}

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

fn denied_response(json: bool, denied: crate::identity::Denied, forged: StatusCode) -> ApiError {
    let code = match denied {
        crate::identity::Denied::Forged(_) => forged,
        crate::identity::Denied::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    ApiError::new(json, code, denied)
}

fn refusal_response(json: bool, refusal: crate::identity::Refusal, absent: StatusCode) -> ApiError {
    match refusal {
        crate::identity::Refusal::Denied(denied) => denied_response(json, denied, absent),
        crate::identity::Refusal::Missing(why) => ApiError::new(json, absent, why),
    }
}

impl axum::extract::FromRequestParts<HttpState> for crate::identity::Caller {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &HttpState,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(crate::identity::Caller::from_headers(
            &state.paths,
            &parts.headers,
        ))
    }
}

fn proven_pen(caller: &crate::identity::Caller, json: bool) -> ApiResult<String> {
    caller
        .pen_or("refused: this needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`")
        .map(ToString::to_string)
        .map_err(|r| refusal_response(json, r, StatusCode::UNAUTHORIZED))
}

fn body_str(doc: &serde_json::Value, name: &str, json: bool) -> ApiResult<String> {
    doc.get(name)
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .ok_or_else(|| ApiError::bad(json, format!("missing `{name}`")))
}

fn reader_from(caller: &crate::identity::Caller) -> crate::store::audience::Reader {
    caller.reader()
}

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

fn match_mode_param(raw: Option<&str>) -> Result<MatchModeParam, String> {
    match raw.map(str::trim).unwrap_or("") {
        "" | "literal" => Ok(MatchModeParam::Literal),
        "regex" => Ok(MatchModeParam::Regex),
        other => Err(format!("bad `mode`: {other}")),
    }
}

fn mode_param(raw: Option<&str>) -> Result<MatchMode, String> {
    match raw.map(str::trim).unwrap_or("") {
        "" | "literal" => Ok(MatchMode::Literal),
        "regex" => Ok(MatchMode::Regex),
        other => Err(format!("bad `mode`: {other}")),
    }
}

fn bool_param(raw: &str, name: &str) -> Result<bool, String> {
    match raw.trim() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        other => Err(format!(
            "bad `{name}`: {other} (want 1/true/yes or 0/false/no)"
        )),
    }
}

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

pub const BOOT_BUDGET_BYTES: usize = 8_192;

#[cfg(test)]
mod front_matter_tests {
    use super::front_matter;

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
mod reply;
pub use reply::*;
mod present;
use present::stats_text;
pub use present::*;
