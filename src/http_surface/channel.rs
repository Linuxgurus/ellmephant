use super::*;

pub(super) fn proven(caller: &crate::identity::Caller) -> Option<String> {
    caller.proven().map(ToString::to_string)
}

pub async fn channel_list(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let pen = proven(&caller);
    let caller = pen.clone();
    let out = server
        .with_read(
            "channel_list",
            crate::store::audience::Reader::everything(),
            move |s| {
                let channels = s.channels()?;
                let unread = match caller.as_deref() {
                    Some(p) => s.channel_unread(p)?,
                    None => Vec::new(),
                };
                let breach = match caller.as_deref() {
                    Some(p) => s.channel_breach(p)?,
                    None => None,
                };
                Ok((channels, unread, breach))
            },
        )
        .await;
    match out {
        Ok((channels, unread, breach)) => present(
            StatusCode::OK,
            json,
            &report(
                (channels, unread, breach, pen),
                |(channels, unread, breach, listener)| {
                    serde_json::json!({
                        "channels": channels,
                        "unread": unread,
                        "breach": breach,
                        "listener": listener,
                    })
                    .to_string()
                },
                |(channels, unread, _, _)| {
                    let text = channels
                        .iter()
                        .map(|c| {
                            let n = unread
                                .iter()
                                .find(|u| u.channel == c.key)
                                .map_or(0, |u| u.count);
                            format!("#{} ({}) gate={} unread={n}", c.key, c.title, c.gate)
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("{text}\n")
                },
            ),
        ),
        Err(e) => Err(ApiError::internal(json, e)),
    }
}

pub async fn channel_history(
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    proven_pen(&caller, json)?;
    let since = params
        .maybe("since")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let limit = params
        .maybe("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(50);
    let key = channel.clone();
    match server
        .with_read(
            "channel_history",
            crate::store::audience::Reader::everything(),
            move |s| s.channel_history(&key, since, limit),
        )
        .await
    {
        Ok(rows) => present(
            StatusCode::OK,
            json,
            &report(
                (channel, rows),
                |(channel, rows)| {
                    serde_json::json!({ "channel": channel, "messages": rows }).to_string()
                },
                |(_, rows)| {
                    let text = rows
                        .iter()
                        .map(|m| format!("{} {}: {}", m.seq, m.pen, m.body))
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("{text}\n")
                },
            ),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_breach(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let Some(pen) = proven(&caller) else {
        return present(
            StatusCode::OK,
            json,
            &report(
                (),
                |()| serde_json::json!({ "breach": null, "proven": false }).to_string(),
                |()| "none (pen not proven)\n".to_string(),
            ),
        );
    };
    let who = pen.clone();
    match server
        .with_read(
            "channel_breach",
            crate::store::audience::Reader::everything(),
            move |s| s.channel_breach(&who),
        )
        .await
    {
        Ok(found) => present(
            StatusCode::OK,
            json,
            &report(
                found,
                |found| serde_json::json!({ "breach": found, "proven": true }).to_string(),
                |found| match found {
                    Some(b) => format!(
                        "#{} seq {} offered {} time(s), {}s ago (gate {})\n",
                        b.channel, b.seq, b.misses, b.age_secs, b.gate
                    ),
                    None => "none\n".to_string(),
                },
            ),
        ),
        Err(e) => Err(ApiError::internal(json, e)),
    }
}

pub async fn channel_send(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let pen = proven_pen(&caller, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        Ok(None) => return Err(ApiError::bad(json, "a JSON body is required")),
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let Some(text) = doc
        .get("body")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
    else {
        return Err(ApiError::new(
            json,
            StatusCode::BAD_REQUEST,
            "`body` is required",
        ));
    };
    let reply_to = doc.get("reply_to").and_then(|v| v.as_i64());
    let (who, chan) = (pen.clone(), channel.clone());
    match server
        .with_store("channel_send", move |s| {
            s.channel_send(&who, &chan, &text, reply_to)
        })
        .await
    {
        Ok(msg) => present(
            StatusCode::OK,
            json,
            &viewed(&msg, |m| format!("sent #{} seq {}\n", m.channel, m.seq)),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_fetch(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let pen = proven_pen(&caller, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => serde_json::json!({}),
    };
    let channel = doc
        .get("channel")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let limit = doc.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
    let who = pen.clone();
    match server
        .with_store("channel_fetch", move |s| {
            s.channel_fetch(&who, channel.as_deref(), limit)
        })
        .await
    {
        Ok(rows) => present(
            StatusCode::OK,
            json,
            &report(
                rows,
                |rows| serde_json::json!({ "fetched": rows.len(), "messages": rows }).to_string(),
                |rows| {
                    let text = rows
                        .iter()
                        .map(|m| format!("#{} {}: {}", m.channel, m.pen, m.body))
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("{text}\n")
                },
            ),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_present(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let pen = proven_pen(&caller, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => serde_json::json!({}),
    };
    let boundary = doc
        .get("boundary")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let who = pen.clone();
    match server
        .with_store("channel_present", move |s| {
            s.channel_present(&who, boundary.as_deref())
        })
        .await
    {
        Ok(marked) => present(
            StatusCode::OK,
            json,
            &report(
                (pen, marked),
                |(pen, marked)| serde_json::json!({ "pen": pen, "marked": marked }).to_string(),
                |(pen, marked)| format!("presented for {pen} ({marked} unread marked)\n"),
            ),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_join(
    State(state): State<HttpState>,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    caller: crate::identity::Caller,
) -> ApiResult<Response> {
    channel_membership(caller, state, channel, headers, raw, true).await
}

pub async fn channel_leave(
    State(state): State<HttpState>,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    caller: crate::identity::Caller,
) -> ApiResult<Response> {
    channel_membership(caller, state, channel, headers, raw, false).await
}

async fn channel_membership(
    caller: crate::identity::Caller,
    state: HttpState,
    channel: String,
    headers: HeaderMap,
    raw: Option<String>,
    join: bool,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let pen = proven_pen(&caller, json)?;
    let (who, chan) = (pen.clone(), channel.clone());
    let Some(server) = state.server() else {
        return Err(ApiError::internal(json, "server not attached"));
    };
    let out = if join {
        server
            .with_store("channel_join", move |s| s.channel_join(&who, &chan))
            .await
    } else {
        server
            .with_store("channel_leave", move |s| s.channel_leave(&who, &chan))
            .await
    };
    match out {
        Ok(()) => present(
            StatusCode::OK,
            json,
            &report(
                (pen, channel, join),
                |(pen, channel, join)| {
                    serde_json::json!({ "pen": pen, "channel": channel, "member": join })
                        .to_string()
                },
                |(_, channel, join)| {
                    format!("{} #{channel}\n", if *join { "joined" } else { "left" })
                },
            ),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_wait(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let Some(pen) = proven(&caller) else {
        return Err(ApiError::new(
            json,
            StatusCode::UNAUTHORIZED,
            "refused: waiting needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`",
        ));
    };
    let after = match params.maybe("after").and_then(|v| v.parse::<i64>().ok()) {
        Some(after) => after,
        None => {
            let who = pen.clone();
            match server
                .with_read(
                    "channel_wait",
                    crate::store::audience::Reader::everything(),
                    move |s| s.channel_head(&who),
                )
                .await
            {
                Ok(head) => head,
                Err(e) => return Err(ApiError::internal(json, e)),
            }
        }
    };
    let timeout = params
        .maybe("timeout")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        .min(25);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    loop {
        let who = pen.clone();
        let rows = match server
            .with_read(
                "channel_wait",
                crate::store::audience::Reader::everything(),
                move |s| s.channel_since(&who, after, 200),
            )
            .await
        {
            Ok(rows) => rows,
            Err(e) => return Err(ApiError::internal(json, e)),
        };
        if !rows.is_empty() || std::time::Instant::now() >= deadline {
            let truncated = rows.len() >= 200;
            return present(
                StatusCode::OK,
                json,
                &report(
                    (after, rows, truncated),
                    |(after, rows, truncated)| {
                        serde_json::json!({ "after": after, "messages": rows, "truncated": truncated })
                            .to_string()
                    },
                    |(_, rows, _)| {
                        let text = rows
                            .iter()
                            .map(|m| format!("#{} {}: {}", m.channel, m.pen, m.body))
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("{text}\n")
                    },
                ),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }
}

pub async fn channel_members(
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    proven_pen(&caller, json)?;
    let key = channel.clone();
    match server
        .with_read(
            "channel_members",
            crate::store::audience::Reader::everything(),
            move |s| s.channel_members(&key),
        )
        .await
    {
        Ok(rows) => present(
            StatusCode::OK,
            json,
            &report(
                (channel, rows),
                |(channel, rows)| {
                    serde_json::json!({ "channel": channel, "members": rows }).to_string()
                },
                |(_, rows)| {
                    let text = rows
                        .iter()
                        .map(|m| {
                            format!(
                                "{} {}{}",
                                m.pen,
                                m.state,
                                if m.explicit { " (explicit)" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("{text}\n")
                },
            ),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_create(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let pen = proven_pen(&caller, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => return Err(ApiError::bad(json, "a JSON body is required")),
    };
    let field = |k: &str| doc.get(k).and_then(|v| v.as_str()).map(ToString::to_string);
    let (Some(channel), Some(title)) = (field("channel"), field("title")) else {
        return Err(ApiError::bad(json, "`channel` and `title` are required"));
    };
    let members = field("members").unwrap_or_else(|| "open".to_string());
    let gate = field("gate").unwrap_or_else(|| "none".to_string());
    let (who, key, title2) = (pen.clone(), channel.clone(), title.clone());
    match server
        .with_store("channel_create", move |s| {
            s.channel_create(&who, &key, &title2, &members, &gate)
        })
        .await
    {
        Ok(row) => present(
            StatusCode::OK,
            json,
            &viewed(&row, |row| {
                format!(
                    "created #{} (members {}, gate {})\n",
                    row.key, row.members, row.gate
                )
            }),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_set_gate(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let pen = proven_pen(&caller, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => return Err(ApiError::bad(json, "a JSON body is required")),
    };
    let Some(gate) = doc
        .get("gate")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
    else {
        return Err(ApiError::new(
            json,
            StatusCode::BAD_REQUEST,
            "`gate` is required",
        ));
    };
    let (who, chan) = (pen.clone(), channel.clone());
    match server
        .with_store("channel_set_gate", move |s| {
            s.channel_set_gate(&who, &chan, &gate)
        })
        .await
    {
        Ok(row) => present(
            StatusCode::OK,
            json,
            &viewed(&row, |row| {
                format!("#{} gate is now {}\n", row.key, row.gate)
            }),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_events(
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    proven_pen(&caller, json)?;
    let key = channel.clone();
    match server
        .with_read(
            "channel_events",
            crate::store::audience::Reader::everything(),
            move |s| s.channel_events(&key),
        )
        .await
    {
        Ok(rows) => present(
            StatusCode::OK,
            json,
            &report(
                (channel, rows),
                |(channel, rows)| {
                    serde_json::json!({ "channel": channel, "events": rows }).to_string()
                },
                |(_, rows)| {
                    let text = rows
                        .iter()
                        .map(|e| {
                            format!(
                                "{} {} {} {} -> {}",
                                e.at,
                                e.pen.clone().unwrap_or_else(|| "-".into()),
                                e.action,
                                e.was.clone().unwrap_or_else(|| "-".into()),
                                e.now.clone().unwrap_or_else(|| "-".into())
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("{text}\n")
                },
            ),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_set_retention(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let pen = proven_pen(&caller, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => return Err(ApiError::bad(json, "a JSON body is required")),
    };
    let Some(keep) = doc.get("keep_secs").and_then(|v| v.as_i64()) else {
        return Err(ApiError::new(
            json,
            StatusCode::BAD_REQUEST,
            "`keep_secs` is required",
        ));
    };
    let (who, chan) = (pen.clone(), channel.clone());
    match server
        .with_store("channel_set_retention", move |s| {
            s.channel_set_retention(&who, &chan, keep)
        })
        .await
    {
        Ok(row) => present(
            StatusCode::OK,
            json,
            &viewed(&row, |row| {
                format!(
                    "#{} keeps {}\n",
                    row.key,
                    if row.keep_secs == 0 {
                        "forever".into()
                    } else {
                        format!("{}s", row.keep_secs)
                    }
                )
            }),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_prune(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let caller = proven_pen(&caller, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => serde_json::json!({}),
    };
    let only = doc
        .get("channel")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let actor = crate::names::Pen::new(&caller).expect("proven pen");
    match server
        .with_store("channel_prune", move |s| {
            crate::ops::channel_prune(s, &actor, only.as_deref())
        })
        .await
    {
        Ok(rows) => present(
            StatusCode::OK,
            json,
            &report(
                rows,
                |rows| {
                    let total: usize = rows.iter().map(|r| r.emptied).sum();
                    serde_json::json!({ "pruned": rows, "emptied": total }).to_string()
                },
                |rows| {
                    let total: usize = rows.iter().map(|r| r.emptied).sum();
                    format!("emptied {total} message body(s)\n")
                },
            ),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

pub async fn channel_invite(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let actor = proven_pen(&caller, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => return Err(ApiError::bad(json, "a JSON body is required")),
    };
    let Some(guest) = doc
        .get("pen")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
    else {
        return Err(ApiError::new(
            json,
            StatusCode::BAD_REQUEST,
            "`pen` is required",
        ));
    };
    let (who, chan, target) = (actor.clone(), channel.clone(), guest.clone());
    match server
        .with_store("channel_invite", move |s| {
            s.channel_invite(&who, &target, &chan)
        })
        .await
    {
        Ok(()) => present(
            StatusCode::OK,
            json,
            &report(
                (channel, guest, actor),
                |(channel, guest, actor)| {
                    serde_json::json!({ "channel": channel, "pen": guest, "member": true, "invited_by": actor })
                        .to_string()
                },
                |(channel, guest, _)| format!("invited {guest} to #{channel}\n"),
            ),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}
