//! Split out of `http_surface.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` is deliberate — the shared vocabulary (the `Server` extractor, `Params`,
//! the response builders) stays in `mod.rs`, so a handler reads the same here as it did there.

use super::*;

/// `GET /projects` — the project index (`project_list`), same bytes.
pub async fn projects(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let mode = match mode_param(params.maybe("mode")) {
        Ok(m) => m,
        Err(e) => return Err(ApiError::bad(json, e)),
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
            let wire = crate::wire::project::list_to_wire_json(&rows).unwrap_or_default();
            present(StatusCode::OK, json, &Table::new("projects", wire))
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

/// `GET /project/{key}` — one project with its ordered preload (`project_get`).
pub async fn project_get(
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let hydrate = match project::Hydrate::parse(params.get("hydrate").unwrap_or("none")) {
        Ok(h) => h,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    match server
        .with_read("project_get", reader, move |s| {
            s.project_view(&key, hydrate)
        })
        .await
    {
        Ok(view) => {
            let wire = crate::wire::project::view_to_wire_json(&view).unwrap_or_default();
            present(StatusCode::OK, json, &Table::new("project", wire))
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

/// `GET /config/{key}` — one config item (`config_get`); `null` when absent or not addressed to you.
pub async fn config_get(
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
    let json = Params::parse(raw.as_deref()).json();
    match server
        .with_read("config_get", reader, move |s| s.config_get(&key))
        .await
    {
        Ok(item) => {
            let wire = serde_json::to_value(&item).unwrap_or(serde_json::Value::Null);
            let text = wire.to_string();
            present(StatusCode::OK, json, &Table::new("config", text))
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

/// `GET /config` — every item this connection may read (`config_list`), with an optional `prefix`.
pub async fn config_list(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
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
            present(StatusCode::OK, json, &Table::new("config", text))
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

/// `POST /config/{key}` — change a value (`config_set`); JSON body `{value, source?}`.
pub async fn config_set(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let who = proven_pen(&caller, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        Ok(None) => return Err(ApiError::bad(json, "a JSON body is required")),
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let value = match doc.get("value").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => return Err(ApiError::bad(json, "missing `value`")),
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
            present(StatusCode::OK, json, &Table::new("config", wire))
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

/// `POST /config` — create an item or rewrite its ACL (`config_define`); JSON body of the tool's args.
pub async fn config_define(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let who = proven_pen(&caller, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        Ok(None) => return Err(ApiError::bad(json, "a JSON body is required")),
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let key = body_str(&doc, "key", json)?;
    let value = body_str(&doc, "value", json)?;
    let set_audience = body_str(&doc, "set_audience", json)?;
    let read_audience = body_str(&doc, "read_audience", json)?;
    let source = body_str(&doc, "source", json)?;
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
            present(StatusCode::OK, json, &Table::new("config", wire))
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}
