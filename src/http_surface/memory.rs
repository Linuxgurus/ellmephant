//! Split out of `http_surface.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` is deliberate — the shared vocabulary (the `Server` extractor, `Params`,
//! the response builders) stays in `mod.rs`, so a handler reads the same here as it did there.

use super::*;

/// `GET /memory/{key}` — one memory whole. Counts a `visits` read, exactly as `memory_get` does: the
/// one write a read may make, because a counter is not data (HTTP.md invariant 1).
pub async fn memory_get(
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let version = match params.parse_as::<i64>("version") {
        Ok(v) => v,
        Err(msg) => return error(json, StatusCode::BAD_REQUEST, msg),
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

/// `GET /unowned` — the rows nobody owns, and why (`memory_unowned`).
pub async fn unowned(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> Response {
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
        Ok(rows) => {
            let mut out = String::new();
            for r in &rows {
                out.push_str(&format!(
                    "  {}  {:<12} {:<10} {}\n",
                    r.key, r.why, r.asserted_by, r.created
                ));
            }
            if rows.is_empty() {
                out.push_str("  (no ownerless rows)\n");
            }
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&rows).unwrap_or_default(),
                out,
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /memory/{key}/backlinks` — incoming `[[…]]` edges (`memory_backlinks`).
pub async fn backlinks(
    server: Server,
    caller: crate::identity::Caller,
    Path(key): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&caller);
    let json = Params::parse(raw.as_deref()).json();
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
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let reader = reader_from(&caller);
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
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let cap = match params.parse_as::<u32>("limit") {
        Ok(v) => v.unwrap_or(HISTORY_LIMIT as u32) as usize,
        Err(msg) => return error(json, StatusCode::BAD_REQUEST, msg),
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
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let search = match search_params(&params) {
        Ok(p) => p,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    match server.run_search(search, reader).await {
        Ok(wire) => render(StatusCode::OK, json, wire.clone(), list_text("hits", &wire)),
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /semantic` — the store-wide search (`semantic_search`): an optional `type`, and
/// `deleted=true` for the graveyard. Same body as the tool, by construction.
pub async fn semantic_search(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> Response {
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
                    return error(
                        json,
                        StatusCode::BAD_REQUEST,
                        format!("bad `fields`: {name}"),
                    );
                }
            }
        }
    }
    // `bool_param` is fallible, and this handler answers `Response` rather than `Result`, so the
    // error becomes a 400 here instead of `?` (which cannot be used in this return type).
    let deleted = match params.maybe("deleted") {
        Some(v) => match bool_param(v, "deleted") {
            Ok(b) => Some(b),
            Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
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
        Ok(wire) => render(StatusCode::OK, json, wire.clone(), list_text("hits", &wire)),
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /dupes` — near-duplicate candidate pairs (`memory_dupes`). `?plan=1` (or `?plan=true`)
/// answers the merge plan instead: one keeper/absorb step per duplicate CLUSTER, and a named refusal
/// for every other pair.
pub async fn dupes(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&caller);
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
    if params.flag("plan") {
        return match server
            .with_read("memory_dupes", reader, move |s| {
                s.dupes_plan(max_distance, limit)
            })
            .await
        {
            Ok(plan) => {
                let wire = crate::wire::dupes::plan_to_wire_json(&plan).unwrap_or_default();
                render(
                    StatusCode::OK,
                    json,
                    wire.clone(),
                    list_text("merge plan", &wire),
                )
            }
            Err(e) => error(json, error_status(&e), e),
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
