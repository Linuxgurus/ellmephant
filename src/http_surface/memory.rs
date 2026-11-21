use super::*;

pub async fn memory_get(
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let version = match params.parse_as::<i64>("version") {
        Ok(v) => v,
        Err(msg) => return Err(ApiError::bad(json, msg)),
    };
    if let Some(n) = version {
        let lookup = key.clone();
        return match server
            .with_read("memory_get", reader.clone(), move |s| s.version(&lookup, n))
            .await
        {
            Ok(Some(past)) => present(
                StatusCode::OK,
                json,
                &MemoryCard {
                    mem: &past,
                    relation: None,
                    version: Some(n),
                },
            ),
            Ok(None) => Err(ApiError::not_found(
                json,
                format!("no such version: {key} {n}"),
            )),
            Err(e) => Err(ApiError::store(json, e)),
        };
    }
    let lookup = key.clone();
    match server
        .with_read("memory_get", reader, move |s| {
            let mem = s.show(&lookup)?;
            let relation = s.owner_relation(&mem);
            Ok((mem, relation))
        })
        .await
    {
        Ok((mut mem, relation)) => {
            if let Some(seen) = server.bump_visit(&key) {
                mem.visits = seen;
            }
            present(
                StatusCode::OK,
                json,
                &MemoryCard {
                    mem: &mem,
                    relation: relation.as_deref(),
                    version: None,
                },
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn unowned(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let limit = params
        .parse_as::<i64>("limit")
        .map(|v| v.unwrap_or(50))
        .unwrap_or(50)
        .clamp(1, 500);
    match server
        .with_read("memory_unowned", reader_from(&caller), move |s| {
            s.unowned(limit)
        })
        .await
    {
        Ok(rows) => present(
            StatusCode::OK,
            json,
            &report(
                rows,
                |rows| serde_json::to_string(rows).unwrap_or_default(),
                |rows| {
                    let mut out = String::new();
                    for r in rows {
                        out.push_str(&format!(
                            "  {}  {:<12} {:<10} {}\n",
                            r.key, r.why, r.asserted_by, r.created
                        ));
                    }
                    if rows.is_empty() {
                        out.push_str("  (no ownerless rows)\n");
                    }
                    out
                },
            ),
        ),
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn backlinks(
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
    let json = Params::parse(raw.as_deref()).json();
    match server
        .with_read("memory_backlinks", reader, move |s| s.backlinks(&key))
        .await
    {
        Ok(rows) => {
            let wire = backlinks_to_wire_json(&rows).unwrap_or_default();
            present(StatusCode::OK, json, &Table::new("backlinks", wire))
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn dangling(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let reader = reader_from(&caller);
    match server
        .with_read("memory_dangling", reader, |s| s.dangling())
        .await
    {
        Ok(rows) => {
            let wire = dangling_to_wire_json(&rows);
            present(StatusCode::OK, json, &Table::new("dangling", wire))
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn history(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let cap = match params.parse_as::<u32>("limit") {
        Ok(v) => v.unwrap_or(HISTORY_LIMIT as u32) as usize,
        Err(msg) => return Err(ApiError::bad(json, msg)),
    };
    let orphans_only = params.flag("orphans_only");
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
                    present(StatusCode::OK, json, &Table::new("versions", wire))
                }
                Err(e) => Err(ApiError::store(json, e)),
            }
        }
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
                present(StatusCode::OK, json, &Table::new("history", wire))
            }
            Err(e) => Err(ApiError::store(json, e)),
        },
    }
}

pub async fn search(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let search = match search_params(&params) {
        Ok(p) => p,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    match server.run_search(search, reader).await {
        Ok(wire) => present(StatusCode::OK, json, &Table::new("hits", wire)),
        Err(e) => Err(ApiError::new(json, status_of_message(&e), e)),
    }
}

pub async fn semantic_search(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
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
                    return Err(ApiError::bad(json, format!("bad `fields`: {name}")));
                }
            }
        }
    }
    let deleted = match params.maybe("deleted") {
        Some(v) => match bool_param(v, "deleted") {
            Ok(b) => Some(b),
            Err(e) => return Err(ApiError::bad(json, e)),
        },
        None => None,
    };
    let p = SemanticSearchParams {
        query: params.maybe("q").map(ToString::to_string),
        doc_type: params.maybe("type").map(ToString::to_string),
        deleted,
        k: params.maybe("k").and_then(|v| v.parse().ok()),
        max_distance: params.maybe("max_distance").and_then(|v| v.parse().ok()),
        fields,
    };
    match server.run_semantic(p, reader).await {
        Ok(wire) => present(StatusCode::OK, json, &Table::new("hits", wire)),
        Err(e) => Err(ApiError::new(json, status_of_message(&e), e)),
    }
}

pub async fn dupes(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let max_distance = match params.parse_as::<f32>("max_distance") {
        Ok(v) => v.unwrap_or(store::dupes::DEFAULT_MAX_DISTANCE),
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let limit = match params.parse_as::<usize>("limit") {
        Ok(v) => v.unwrap_or(DUPES_LIMIT),
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    if params.flag("plan") {
        return match server
            .with_read("memory_dupes", reader, move |s| {
                s.dupes_plan(max_distance, limit)
            })
            .await
        {
            Ok(plan) => {
                let wire = crate::wire::dupes::plan_to_wire_json(&plan).unwrap_or_default();
                present(StatusCode::OK, json, &Table::new("merge plan", wire))
            }
            Err(e) => Err(ApiError::store(json, e)),
        };
    }
    match server
        .with_read("memory_dupes", reader, move |s| {
            s.dupes(max_distance, limit)
        })
        .await
    {
        Ok(pairs) => {
            let wire = crate::wire::dupes::to_wire_json(&pairs).unwrap_or_default();
            present(StatusCode::OK, json, &Table::new("dupes", wire))
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}
