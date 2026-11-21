use super::*;

pub async fn not_found(uri: axum::http::Uri) -> ApiResult<Response> {
    Err(ApiError::not_found(
        false,
        format!("not found: {}", uri.path()),
    ))
}

pub async fn method_not_allowed() -> ApiResult<Response> {
    Err(ApiError::new(
        false,
        StatusCode::METHOD_NOT_ALLOWED,
        "method not allowed",
    ))
}

pub(super) fn api_op(route: &Route, card: &crate::mcp::ToolCard, full: bool) -> serde_json::Value {
    let ann = &card.annotations;
    let flag = |name: &str| ann.get(name).and_then(serde_json::Value::as_bool);
    let read_only = flag("readOnlyHint")
        .or(flag("read_only_hint"))
        .unwrap_or(false);
    let destructive = flag("destructiveHint")
        .or(flag("destructive_hint"))
        .unwrap_or(false);
    let description = if full {
        card.description.clone()
    } else {
        card.description
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect()
    };
    let mut op = serde_json::json!({
        "op": route.tool,
        "group": crate::mcp::group_of(route.tool),
        "method": route.method,
        "path": route.path,
        "core": crate::mcp::is_core(route.tool),
        "read_only": read_only,
        "destructive": destructive,
        "description": description,
    });
    if full {
        op["schema"] = card.input_schema.clone();
    }
    op
}

pub(super) fn api_doc(group: Option<&str>, full: bool) -> Option<serde_json::Value> {
    let cards = tool_catalog();
    let ops: Vec<serde_json::Value> = ROUTES
        .iter()
        .filter_map(|route| {
            let card = cards.iter().find(|c| c.name == route.tool)?;
            let g = crate::mcp::group_of(route.tool);
            if group.is_some_and(|want| want != g) {
                return None;
            }
            Some(api_op(route, card, full))
        })
        .collect();
    if group.is_some() && ops.is_empty() {
        return None;
    }
    let mut groups = std::collections::BTreeMap::<&str, usize>::new();
    for route in ROUTES {
        *groups.entry(crate::mcp::group_of(route.tool)).or_default() += 1;
    }
    let mut doc = serde_json::json!({
        "surface": "core",
        "endpoint": "/mcp/core",
        "expanded": "/mcp",
        "note": "The core door advertises the `core` group only. Every op here is reachable over REST at the route shown; `?full=true` adds each argument schema. The expanded door (/mcp) advertises all of them.",
        "groups": groups,
        "ops": ops,
    });
    if let Some(g) = group {
        doc["group"] = serde_json::Value::String(g.to_string());
    }
    Some(doc)
}

pub async fn api_index(RawQuery(raw): RawQuery) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let full = params.flag("full");
    match api_doc(None, full) {
        Some(doc) => present(
            StatusCode::OK,
            params.json(),
            &report(
                doc,
                |doc| json_ok(doc.clone()),
                |_| format!("{} ops; add ?full=true for schemas\n", ROUTES.len()),
            ),
        ),
        None => Err(ApiError::internal(params.json(), "the API index is empty")),
    }
}

pub async fn api_group(Path(group): Path<String>, RawQuery(raw): RawQuery) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let full = params.flag("full");
    match api_doc(Some(&group), full) {
        Some(doc) => present(
            StatusCode::OK,
            params.json(),
            &report(
                (doc, group),
                |(doc, _)| json_ok(doc.clone()),
                |(_, group)| format!("group {group}; add ?full=true for schemas\n"),
            ),
        ),
        None => Err(ApiError::not_found(
            params.json(),
            format!("no group {group:?}: try /api"),
        )),
    }
}
