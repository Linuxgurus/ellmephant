use super::*;

pub async fn upsert(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let wait = matches!(params.get("wait"), Some("true") | Some("1"));
    let scope = authorize(&state, &headers, json)?;
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let patch = if is_json_content(&headers) {
        memory_patch_json(&bytes)
    } else {
        memory_patch_text(&String::from_utf8_lossy(&bytes))
    };
    let (patch, as_pen) = match patch {
        Ok(p) => p,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let caller = caller
        .pen()
        .map_err(|r| refusal_response(json, r, StatusCode::UNAUTHORIZED))?
        .clone();
    let credit = match as_pen.as_deref().map(crate::names::Pen::new).transpose() {
        Ok(credit) => credit,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let cmd = crate::ops::Upsert { patch, credit };
    match server
        .with_store("memory_upsert", move |s| {
            crate::ops::memory_upsert(s, &caller, cmd)
        })
        .await
    {
        Ok(out) => {
            if let Some(current) = out.conflict {
                let key = out.key;
                tracing::info!(
                    area = scope.area,
                    key = %key,
                    version = current,
                    "compare-and-swap write refused over http"
                );
                return present(
                    StatusCode::CONFLICT,
                    json,
                    &report(
                        (key, current),
                        |(key, current)| {
                            json_ok(serde_json::json!({
                                "status": "conflict",
                                "key": key,
                                "current_version": current,
                            }))
                        },
                        |(key, current)| {
                            format!("conflict: {key} is at version {current}; nothing written")
                        },
                    ),
                );
            }
            let key = out.key.clone();
            let mut result = serde_json::json!({
                "status": if out.created { "created" } else { "updated" },
                "key": key.clone(),
                "queued": out.queued,
            });
            let mut indexed: Option<bool> = None;
            if wait {
                indexed = Some(match server.encode_now(&out.key).await {
                    Ok(indexed) => indexed,
                    Err(err) => {
                        tracing::warn!(key = %out.key, error = %err, "?wait=true could not index");
                        result["encode_error"] = serde_json::json!(err.to_string());
                        false
                    }
                });
                result["indexed"] = serde_json::json!(indexed);
            }
            tracing::info!(area = scope.area, key = %key, "memory stored over http");
            present(
                StatusCode::OK,
                json,
                &report(
                    (result, key, out.queued, indexed),
                    |(result, _, _, _)| json_ok(result.clone()),
                    |(_, key, queued, indexed)| match indexed {
                        Some(indexed) => {
                            format!("stored {key} (queued={queued}, indexed={indexed})")
                        }
                        None => format!("stored {key} (queued={queued})"),
                    },
                ),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn memory_delete(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let pen = match caller.pen() {
        Ok(name) => name.clone(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    let lookup = key.clone();
    match server
        .with_store("memory_delete", move |s| {
            crate::ops::memory_delete(s, &pen, &lookup)
        })
        .await
    {
        Ok(Some(remaining)) => {
            tracing::info!(area = scope.area, key = %key, versions = remaining, "memory removed over http");
            present(
                StatusCode::OK,
                json,
                &report(
                    (key, remaining),
                    |(key, remaining)| {
                        json_ok(serde_json::json!({
                            "status": "removed",
                            "key": key,
                            "versions_remaining": remaining,
                        }))
                    },
                    |(key, remaining)| {
                        format!(
                            "deleted {key} ({remaining} version(s) remain, the newest being the state it had)"
                        )
                    },
                ),
            )
        }
        Ok(None) => Err(ApiError::not_found(json, format!("no such memory: {key}"))),
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn history_search(
    State(state): State<HttpState>,
    server: Server,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let Some(phrase) = params.get("q").map(ToString::to_string) else {
        return Err(ApiError::new(
            true,
            StatusCode::BAD_REQUEST,
            "need `q` (words to find)",
        ));
    };
    let limit = params
        .get("k")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10)
        .clamp(1, 200);
    let scope = authorize(&state, &headers, true)?;
    match server
        .with_store("memory_history_search", move |s| {
            s.history_search(&phrase, limit)
        })
        .await
    {
        Ok(hits) => {
            let rows: Vec<serde_json::Value> = hits
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "key": h.key,
                        "version": h.n,
                        "written_at": h.written_at,
                        "orphan": h.orphan,
                        "state": "past",
                        "snippet": h.snippet,
                    })
                })
                .collect();
            tracing::info!(
                area = scope.area,
                hits = rows.len(),
                "history searched over http"
            );
            present(
                StatusCode::OK,
                true,
                &report(
                    rows,
                    |rows| json_ok(serde_json::json!({ "hits": rows })),
                    |rows| format!("{} past state(s) matched", rows.len()),
                ),
            )
        }
        Err(e) => Err(ApiError::store(true, e)),
    }
}

pub async fn history_erase(
    State(state): State<HttpState>,
    server: Server,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let ask_key = params.get("key").map(ToString::to_string);
    let ask_orphans = matches!(params.get("orphans"), Some("true") | Some("1"));
    let ask_confirm = matches!(params.get("confirm"), Some("true") | Some("1"));
    let json = true;
    let scope = authorize(&state, &headers, json)?;
    if !ask_confirm {
        return Err(ApiError::bad(
            json,
            "this destroys text and nothing can restore it: pass confirm=true",
        ));
    }
    match (ask_key, ask_orphans) {
        (Some(key), false) => {
            let lookup = key.clone();
            match server
                .with_store("memory_history_erase", move |s| s.erase_history(&lookup))
                .await
            {
                Ok(n) => {
                    tracing::info!(area = scope.area, key = %key, versions = n, "history erased over http");
                    present(
                        StatusCode::OK,
                        json,
                        &report(
                            (key, n),
                            |(key, n)| {
                                json_ok(serde_json::json!({
                                    "status": "erased",
                                    "key": key,
                                    "versions_erased": n,
                                }))
                            },
                            |(key, n)| format!("erased {n} version(s) of {key}"),
                        ),
                    )
                }
                Err(e) => Err(ApiError::store(json, e)),
            }
        }
        (None, true) => match server
            .with_store("memory_history_erase", move |s| s.erase_orphan_history())
            .await
        {
            Ok((keys, versions)) => {
                tracing::info!(
                    area = scope.area,
                    keys,
                    versions,
                    "orphan history erased over http"
                );
                present(
                    StatusCode::OK,
                    json,
                    &report(
                        (keys, versions),
                        |(keys, versions)| {
                            json_ok(serde_json::json!({
                                "status": "erased",
                                "orphan_keys": keys,
                                "versions_erased": versions,
                            }))
                        },
                        |(keys, versions)| {
                            format!("erased {versions} version(s) of {keys} deleted key(s)")
                        },
                    ),
                )
            }
            Err(e) => Err(ApiError::store(json, e)),
        },
        _ => Err(ApiError::bad(
            json,
            "pass exactly one of key=<slug> or orphans=true",
        )),
    }
}

pub async fn upvote(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let body = match json_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let keys = match body.as_ref().and_then(|b| b.get("keys")) {
        Some(value) => match serde_json::from_value::<crate::mcp::OneOrMany>(value.clone()) {
            Ok(parsed) => parsed.into_vec(),
            Err(_) => {
                return Err(ApiError::bad(
                    json,
                    format!("bad `keys`: {value} (a string or an array of strings expected)"),
                ));
            }
        },
        None => vec![key.clone()],
    };
    let named = keys.join(", ");
    match crate::mcp::upvote_sweep(&server, reader_from(&caller), keys).await {
        Ok(payload) => {
            tracing::info!(area = scope.area, keys = %named, "upvoted over http");
            present(
                StatusCode::OK,
                json,
                &report(
                    (payload, named),
                    |(payload, _)| payload.clone(),
                    |(_, named)| format!("upvoted {named}"),
                ),
            )
        }
        Err(message) => {
            let status = status_of_message(&message);
            Err(ApiError::new(json, status, message))
        }
    }
}

pub async fn append(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let scope = authorize(&state, &headers, json)?;
    let body = match json_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let line = match body
        .as_ref()
        .and_then(|b| b.get("line"))
        .and_then(|l| l.as_str())
    {
        Some(line) => line.to_string(),
        None => match params.maybe("line") {
            Some(line) => line.to_string(),
            None => {
                return Err(ApiError::bad(
                    json,
                    "missing `line`: send a JSON body {\"line\": \"...\"}, or `?line=...`",
                ));
            }
        },
    };
    let pen = match caller.pen() {
        Ok(name) => name.clone(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    let force = params.flag("force");
    let target = key.clone();
    match server
        .with_store("memory_append", move |s| {
            crate::ops::memory_append(s, &pen, &target, &line, force)
        })
        .await
    {
        Ok(out) => {
            tracing::info!(area = scope.area, key = %key, appended = out.appended, "appended over http");
            present(
                StatusCode::OK,
                json,
                &report(
                    (out, key),
                    |(out, _)| serde_json::to_string(out).unwrap_or_default(),
                    |(out, key)| {
                        if out.appended {
                            format!("appended one line to {key}")
                        } else {
                            format!("{key} already carries that line — nothing written")
                        }
                    },
                ),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn merge(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let scope = authorize(&state, &headers, json)?;
    let body = match json_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    if let Some(given) = body
        .as_ref()
        .and_then(|b| b.get("from"))
        .and_then(|f| f.as_str())
        && given != key
    {
        return Err(ApiError::bad(
            json,
            format!("body `from` is `{given}` but the path names `{key}`"),
        ));
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
                return Err(ApiError::bad(
                    json,
                    "missing `into`: send `?into=<key>`, or a JSON body {\"into\": \"<key>\"}",
                ));
            }
        },
    };
    let from = key.clone();
    let target = into.clone();
    let pen = match caller.pen() {
        Ok(name) => name.clone(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    match server
        .with_store("memory_merge", move |s| {
            crate::ops::memory_merge(s, &pen, &from, &target)
        })
        .await
    {
        Ok(out) => {
            tracing::info!(area = scope.area, from = %key, into = %into, "merged over http");
            present(
                StatusCode::OK,
                json,
                &report(
                    (out, key, into),
                    |(out, _, _)| serde_json::to_string(out).unwrap_or_default(),
                    |(out, key, into)| {
                        format!(
                            "merged {key} -> {into} ({} line(s) integrated, {} refs repointed)",
                            out.integrated.len(),
                            out.bodies
                        )
                    },
                ),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn fold(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let scope = authorize(&state, &headers, json)?;
    let body = match json_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    if let Some(given) = body
        .as_ref()
        .and_then(|b| b.get("from"))
        .and_then(|f| f.as_str())
        && given != key
    {
        return Err(ApiError::bad(
            json,
            format!("body `from` is `{given}` but the path names `{key}`"),
        ));
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
                return Err(ApiError::bad(
                    json,
                    "missing `into`: send `?into=<key>`, or a JSON body {\"into\": \"<key>\"}",
                ));
            }
        },
    };
    let from = key.clone();
    let target = into.clone();
    let pen = match caller.pen() {
        Ok(name) => name.clone(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    match server
        .with_store("memory_fold", move |s| {
            crate::ops::memory_fold(s, &pen, &from, &target)
        })
        .await
    {
        Ok(out) => {
            tracing::info!(area = scope.area, from = %key, into = %into, changed = out.changed, "folded over http");
            present(
                StatusCode::OK,
                json,
                &report(
                    (out, key, into),
                    |(out, _, _)| serde_json::to_string(out).unwrap_or_default(),
                    |(out, key, into)| {
                        format!(
                            "folded {key} into {into} ({} line(s) appended{})",
                            out.integrated.len(),
                            if out.changed {
                                ""
                            } else {
                                "; nothing was missing"
                            }
                        )
                    },
                ),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn adopt(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let scope = authorize(&state, &headers, json)?;
    let body = match json_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let force = body
        .as_ref()
        .and_then(|b| b.get("force"))
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| matches!(params.get("force"), Some("true") | Some("1")));
    let pen = match caller.pen_or("adopt needs a proven identity (X-Stet-From + X-Stet-Auth)") {
        Ok(pen) => pen.clone(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    let adopting = key.clone();
    let actor = pen.clone();
    match server
        .with_store("memory_adopt", move |s| {
            crate::ops::memory_adopt(s, &actor, &adopting, None, force)
        })
        .await
    {
        Ok(()) => {
            tracing::info!(area = scope.area, key = %key, owner = %pen, "adopted over http");
            present(
                StatusCode::OK,
                json,
                &report(
                    (key, pen),
                    |(key, pen)| {
                        serde_json::json!({
                            "status": "adopted",
                            "key": key,
                            "owner": pen.as_str(),
                        })
                        .to_string()
                    },
                    |(key, pen)| format!("adopted {key} as {pen}"),
                ),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn reparse(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let caller = proven_pen(&caller, json)?;
    match server
        .with_store("memory_reparse", move |s| s.reparse())
        .await
    {
        Ok(report) => {
            tracing::info!(
                area = scope.area,
                pen = %caller,
                rows = report.rows,
                edges = report.edges,
                "re-parsed the link graph over http"
            );
            present(
                StatusCode::OK,
                json,
                &super::report(
                    report,
                    |report| {
                        json_ok(serde_json::json!({
                            "status": "reparsed",
                            "rows": report.rows,
                            "edges": report.edges,
                        }))
                    },
                    |report| {
                        format!(
                            "re-parsed {} document(s); the graph now holds {} edge(s)",
                            report.rows, report.edges
                        )
                    },
                ),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn reembed(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let caller = proven_pen(&caller, json)?;
    match server
        .with_store("memory_reembed", move |s| s.reembed())
        .await
    {
        Ok(report) => {
            tracing::info!(
                area = scope.area,
                pen = %caller,
                queued = report.queued,
                "queued a re-embed over http"
            );
            present(
                StatusCode::OK,
                json,
                &super::report(
                    report,
                    |report| {
                        json_ok(serde_json::json!({
                            "status": "queued",
                            "queued": report.queued,
                            "dim": report.dim,
                            "rebuilt": report.rebuilt,
                            "model": report.model,
                        }))
                    },
                    |report| {
                        format!(
                            "queued {} document(s) for re-embedding ({} rebuilt)",
                            report.queued, report.rebuilt
                        )
                    },
                ),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn project_upsert(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let patch = if is_json_content(&headers) {
        project_patch_json(&bytes)
    } else {
        project_patch_text(&String::from_utf8_lossy(&bytes))
    };
    let patch = match patch {
        Ok(p) => p,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let proven = match caller.pen() {
        Ok(name) => name.clone(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    match server
        .with_store("project_upsert", move |s| {
            crate::ops::project_upsert(s, &proven, &patch)
        })
        .await
    {
        Ok(out) => {
            tracing::info!(area = scope.area, key = %out.key, "project stored over http");
            present(
                StatusCode::OK,
                json,
                &report(
                    out,
                    |out| {
                        json_ok(serde_json::json!({
                            "status": if out.created { "created" } else { "updated" },
                            "key": out.key,
                        }))
                    },
                    |out| {
                        format!(
                            "stored project {} ({})",
                            out.key,
                            if out.created { "created" } else { "updated" }
                        )
                    },
                ),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}
