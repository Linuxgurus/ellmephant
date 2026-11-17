//! Split out of `http_surface.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` is deliberate — the shared vocabulary (the `Server` extractor, `Params`,
//! the response builders) stays in `mod.rs`, so a handler reads the same here as it did there.

use super::*;

/// `GET /projects` — the project index (`project_list`), same bytes.
pub async fn projects(
    State(state): State<HttpState>,
    server: Server,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers, &state.paths);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let mode = match mode_param(params.maybe("mode")) {
        Ok(m) => m,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let filter = project::Filter {
        status: params.maybe("status").map(ToString::to_string),
        tags: params.all("tag"),
        owner: params.all("owner"),
        pattern: params.maybe("pattern").map(ToString::to_string),
        mode,
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
    server: Server,
    headers: HeaderMap,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers, &state.paths);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let hydrate = match project::Hydrate::parse(params.get("hydrate").unwrap_or("none")) {
        Ok(h) => h,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e.to_string()),
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

/// `GET /config/{key}` — one config item (`config_get`); `null` when absent or not addressed to you.
pub async fn config_get(
    State(state): State<HttpState>,
    server: Server,
    headers: HeaderMap,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers, &state.paths);
    let json = Params::parse(raw.as_deref()).json();
    match server
        .with_read("config_get", reader, move |s| s.config_get(&key))
        .await
    {
        Ok(item) => {
            let wire = serde_json::to_value(&item).unwrap_or(serde_json::Value::Null);
            let text = wire.to_string();
            render(
                StatusCode::OK,
                json,
                text.clone(),
                list_text("config", &text),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /config` — every item this connection may read (`config_list`), with an optional `prefix`.
pub async fn config_list(
    State(state): State<HttpState>,
    server: Server,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&headers, &state.paths);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let prefix = params.maybe("prefix").map(ToString::to_string);
    match server
        .with_read("config_list", reader, move |s| {
            s.config_list(prefix.as_deref())
        })
        .await
    {
        Ok(items) => {
            let wire = serde_json::json!({ "config": items });
            let text = wire.to_string();
            render(
                StatusCode::OK,
                json,
                text.clone(),
                list_text("config", &text),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /config/{key}` — change a value (`config_set`); JSON body `{value, source?}`.
pub async fn config_set(
    State(state): State<HttpState>,
    server: Server,
    headers: HeaderMap,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let who = match proven_pen(&headers, json, &state.paths) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        Ok(None) => return error(json, StatusCode::BAD_REQUEST, "a JSON body is required"),
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let value = match doc.get("value").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => return error(json, StatusCode::BAD_REQUEST, "missing `value`"),
    };
    let source = doc
        .get("source")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let log_key = key.clone();
    match server
        .with_store("config_set", move |s| {
            s.config_set(&who, &key, &value, source.as_deref())
        })
        .await
    {
        Ok(item) => {
            tracing::info!(area = scope.area, key = %log_key, "config written over http");
            let wire = serde_json::to_string(&item).unwrap_or_default();
            render(
                StatusCode::OK,
                json,
                wire.clone(),
                list_text("config", &wire),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /config` — create an item or rewrite its ACL (`config_define`); JSON body of the tool's args.
pub async fn config_define(
    State(state): State<HttpState>,
    server: Server,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let who = match proven_pen(&headers, json, &state.paths) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        Ok(None) => return error(json, StatusCode::BAD_REQUEST, "a JSON body is required"),
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let key = match body_str(&doc, "key", json) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let value = match body_str(&doc, "value", json) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let set_audience = match body_str(&doc, "set_audience", json) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let read_audience = match body_str(&doc, "read_audience", json) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let source = match body_str(&doc, "source", json) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let log_key = key.clone();
    match server
        .with_store("config_define", move |s| {
            s.config_define(&who, &key, &value, &set_audience, &read_audience, &source)
        })
        .await
    {
        Ok(item) => {
            tracing::info!(area = scope.area, key = %log_key, "config defined over http");
            let wire = serde_json::to_string(&item).unwrap_or_default();
            render(
                StatusCode::OK,
                json,
                wire.clone(),
                list_text("config", &wire),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}
