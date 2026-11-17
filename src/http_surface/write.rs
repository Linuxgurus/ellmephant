//! Split out of `http_surface.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` is deliberate — the shared vocabulary (the `Server` extractor, `Params`,
//! the response builders) stays in `mod.rs`, so a handler reads the same here as it did there.

use super::*;

/// `POST /memory` — store one memory (`memory_upsert`); text front-matter or JSON.
pub async fn upsert(
    State(state): State<HttpState>,
    server: Server,
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
    let (patch, as_pen) = match patch {
        Ok(p) => p,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    // ★ THE COMMENT ABOVE WAS TRUE AND THE CODE WAS NOT: the parsers return `asserted_by: None` and
    // nobody stamped it, so every row written over HTTP landed in the COMMONS — owner NULL — and
    // since #119 that means any pen may rewrite it. The bearer proves the door; the identity pair
    // names the pen, and only a PROVEN one is stamped (#109).
    // ★ ONE CALLER, AND A WRITE WITHOUT ONE IS REFUSED (soma/ellmephant#231) — including a FORGED
    // claim, which `.ok().flatten()` used to collapse into "nobody" and then write as the commons.
    let caller = match crate::identity::Caller::from_headers(&state.paths, &headers).pen() {
        Ok(name) => name.clone(),
        Err(r) => return refusal_response(json, r, StatusCode::UNAUTHORIZED),
    };
    let credit = match as_pen.as_deref().map(crate::names::Pen::new).transpose() {
        Ok(credit) => credit,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e.to_string()),
    };
    let cmd = crate::ops::Upsert { patch, credit };
    match server
        .with_store("memory_upsert", move |s| {
            crate::ops::memory_upsert(s, &caller, cmd)
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
    server: Server,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    // The bearer proves the DOOR; the identity pair names the PEN. A removal with no proven pen has
    // nobody to name in the ledger — and since #231 it is REFUSED, not passed as `None`.
    let pen = match crate::identity::Caller::from_headers(&state.paths, &headers).pen() {
        Ok(name) => name.clone(),
        Err(r) => return refusal_response(json, r, StatusCode::UNAUTHORIZED),
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

/// `GET /history/search?q=<words>` — exact words over every PAST state (`memory_history_search`).
pub async fn history_search(
    State(state): State<HttpState>,
    server: Server,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let Some(phrase) = params.get("q").map(ToString::to_string) else {
        return error(true, StatusCode::BAD_REQUEST, "need `q` (words to find)");
    };
    let limit = params
        .get("k")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(10)
        .clamp(1, 200);
    // Reads need no bearer beyond the door's own, like every other GET on this surface.
    let scope = match authorize(&state, &headers, true) {
        Ok(s) => s,
        Err(r) => return r,
    };
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
            render(
                StatusCode::OK,
                true,
                json_ok(serde_json::json!({ "hits": rows })),
                format!("{} past state(s) matched", rows.len()),
            )
        }
        Err(e) => error(true, error_status(&e), e),
    }
}

/// `DELETE /history?key=<slug>` or `?orphans=true` — erase HISTORY (`memory_history_erase`).
///
/// The store's only act that destroys text. `memory_delete` snapshots the row and keeps its versions,
/// so a deleted key still holds its body in full; this is the door out. `confirm=true` is required on
/// both shapes, and a LIVE key is refused: its history is the only copy of what it said.
pub async fn history_erase(
    State(state): State<HttpState>,
    server: Server,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let ask_key = params.get("key").map(ToString::to_string);
    let ask_orphans = matches!(params.get("orphans"), Some("true") | Some("1"));
    let ask_confirm = matches!(params.get("confirm"), Some("true") | Some("1"));
    let json = true;
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if !ask_confirm {
        return error(
            json,
            StatusCode::BAD_REQUEST,
            "this destroys text and nothing can restore it: pass confirm=true",
        );
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
                    render(
                        StatusCode::OK,
                        json,
                        json_ok(serde_json::json!({
                            "status": "erased",
                            "key": key,
                            "versions_erased": n,
                        })),
                        format!("erased {n} version(s) of {key}"),
                    )
                }
                Err(e) => error(json, error_status(&e), e),
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
                render(
                    StatusCode::OK,
                    json,
                    json_ok(serde_json::json!({
                        "status": "erased",
                        "orphan_keys": keys,
                        "versions_erased": versions,
                    })),
                    format!("erased {versions} version(s) of {keys} deleted key(s)"),
                )
            }
            Err(e) => error(json, error_status(&e), e),
        },
        _ => error(
            json,
            StatusCode::BAD_REQUEST,
            "pass exactly one of key=<slug> or orphans=true",
        ),
    }
}

/// `POST /memory/{key}/upvote` — mark one memory useful (`memory_upvote`).
pub async fn upvote(
    State(state): State<HttpState>,
    server: Server,
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
    // ★ ONE KEY OR MANY (#mem): the tool takes either shape, so the route must too — a client that
    // sends `{"keys":"one"}` is asking the same question as `{"keys":["one"]}`.
    let keys = match body.as_ref().and_then(|b| b.get("keys")) {
        Some(value) => match serde_json::from_value::<crate::mcp::OneOrMany>(value.clone()) {
            Ok(parsed) => parsed.into_vec(),
            Err(_) => {
                return error(
                    json,
                    StatusCode::BAD_REQUEST,
                    format!("bad `keys`: {value} (a string or an array of strings expected)"),
                );
            }
        },
        None => vec![key.clone()],
    };
    let named = keys.join(", ");
    // One implementation, two doors: the tool and this route share `upvote_sweep`, so neither the
    // errors nor the JSON can drift apart.
    match crate::mcp::upvote_sweep(&server, reader_from(&headers, &state.paths), keys).await {
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

/// `POST /memory/{key}/append` — continue a subject with one line (`memory_append`).
///
/// Body: `{"line": "..."}`. The line is the whole payload; there is no body resend, which is the
/// entire point of the verb.
pub async fn append(
    State(state): State<HttpState>,
    server: Server,
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
    let body = match json_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
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
                return error(
                    json,
                    StatusCode::BAD_REQUEST,
                    "missing `line`: send a JSON body {\"line\": \"...\"}, or `?line=...`",
                );
            }
        },
    };
    let pen = match crate::identity::Caller::from_headers(&state.paths, &headers).pen() {
        Ok(name) => name.clone(),
        Err(r) => return refusal_response(json, r, StatusCode::UNAUTHORIZED),
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
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&out).unwrap_or_default(),
                if out.appended {
                    format!("appended one line to {key}")
                } else {
                    format!("{key} already carries that line — nothing written")
                },
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /memory/{key}/merge?into=` — integrate one memory into another and delete it (`memory_merge`).
pub async fn merge(
    State(state): State<HttpState>,
    server: Server,
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
    let from = key.clone();
    let target = into.clone();
    // A merge DESTROYS the source; the pen the identity pair proves is the one whose word it is.
    let pen = match crate::identity::Caller::from_headers(&state.paths, &headers).pen() {
        Ok(name) => name.clone(),
        Err(r) => return refusal_response(json, r, StatusCode::UNAUTHORIZED),
    };
    match server
        .with_store("memory_merge", move |s| {
            crate::ops::memory_merge(s, &pen, &from, &target)
        })
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

/// `POST /memory/{key}/fold?into=` — append one memory's lines to another (`memory_fold`).
///
/// The cross-pen maintenance verb: `from` (the path key) keeps existing, and `into` is appended to
/// without needing its write authority. The two rows' arguments are parsed exactly as `merge`'s so a
/// caller can send the MCP tool's arguments unchanged.
pub async fn fold(
    State(state): State<HttpState>,
    server: Server,
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
    let from = key.clone();
    let target = into.clone();
    // A fold changes the record, so it takes the same channel gate a merge does; it does NOT take
    // the source's write authority, which is the whole point. It DOES require a proven pen: the
    // store refuses a fold with no actor (an append nobody is named for), so since #231 the door
    // refuses first rather than letting an unproven caller reach `fold_into`.
    let pen = match crate::identity::Caller::from_headers(&state.paths, &headers).pen() {
        Ok(name) => name.clone(),
        Err(r) => return refusal_response(json, r, StatusCode::UNAUTHORIZED),
    };
    match server
        .with_store("memory_fold", move |s| {
            crate::ops::memory_fold(s, &pen, &from, &target)
        })
        .await
    {
        Ok(out) => {
            tracing::info!(area = scope.area, from = %key, into = %into, changed = out.changed, "folded over http");
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&out).unwrap_or_default(),
                format!(
                    "folded {key} into {into} ({} line(s) appended{})",
                    out.integrated.len(),
                    if out.changed {
                        ""
                    } else {
                        "; nothing was missing"
                    }
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /memory/{key}/adopt` — take responsibility for a memory (`memory_adopt`).
///
/// Responsibility, not authorship: `asserted_by` is untouched. An ownerless row needs no `force`;
/// an owned row needs `force=true` AND the caller to be an ancestor of its owner.
pub async fn adopt(
    State(state): State<HttpState>,
    server: Server,
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
    let body = match json_body(&headers, body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let force = body
        .as_ref()
        .and_then(|b| b.get("force"))
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| matches!(params.get("force"), Some("true") | Some("1")));
    let pen = match crate::identity::Caller::from_headers(&state.paths, &headers)
        .pen_or("adopt needs a proven identity (X-Stet-From + X-Stet-Auth)")
    {
        Ok(pen) => pen.to_string(),
        Err(r) => return refusal_response(json, r, StatusCode::UNAUTHORIZED),
    };
    let adopting = key.clone();
    let owner = pen.clone();
    let actor = pen.clone();
    match server
        .with_store("memory_adopt", move |s| {
            s.channel_gate(&actor, crate::store::channel::Class::Record)?;
            s.adopt(&adopting, &owner, force)
        })
        .await
    {
        Ok(()) => {
            tracing::info!(area = scope.area, key = %key, owner = %pen, "adopted over http");
            render(
                StatusCode::OK,
                json,
                serde_json::json!({
                    "status": "adopted",
                    "key": key,
                    "owner": pen,
                })
                .to_string(),
                format!("adopted {key} as {pen}"),
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
    server: Server,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let caller = match proven_pen(&headers, json, &state.paths) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match server
        .with_store("memory_reparse", move |s| {
            // ★ A PEN IS REQUIRED, A GATE IS NOT (#168 review): identity is what makes the call
            // attributable, but a rebuild is derived data — it is not "acting", and a locked pen may
            // still repair the graph. So `caller` is logged, not consulted.
            s.reparse()
        })
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
    server: Server,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let caller = match proven_pen(&headers, json, &state.paths) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match server
        .with_store("memory_reembed", move |s| {
            // ★ A PEN IS REQUIRED, A GATE IS NOT (#168 review): same rule as `reparse` above — a
            // rebuild is derived data, not an act on the world.
            s.reembed()
        })
        .await
    {
        Ok(report) => {
            tracing::info!(
                area = scope.area,
                pen = %caller,
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
    server: Server,
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
    // The pen the identity pair proves. A project write is a write: since #231 it needs one, the
    // same as on the MCP door, so the channel gate can bite and the row records who stored it.
    let proven = match crate::identity::Caller::from_headers(&state.paths, &headers).pen() {
        Ok(name) => name.clone(),
        Err(r) => return refusal_response(json, r, StatusCode::UNAUTHORIZED),
    };
    match server
        .with_store("project_upsert", move |s| {
            crate::ops::project_upsert(s, &proven, &patch)
        })
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
