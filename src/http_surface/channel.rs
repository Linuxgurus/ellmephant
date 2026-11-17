//! Split out of `http_surface.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` is deliberate — the shared vocabulary (the `Server` extractor, `Params`,
//! the response builders) stays in `mod.rs`, so a handler reads the same here as it did there.

use super::*;

/// Who is asking, if anyone proved it. Reads are open, so `None` is a legitimate answer.
pub(super) fn proven(caller: &crate::identity::Caller) -> Option<String> {
    caller.proven().map(ToString::to_string)
}

/// GET /channels — policy for every channel, plus this pen's unread counts and breach.
pub async fn channel_list(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> Response {
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
        Ok((channels, unread, breach)) => {
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
            render(
                StatusCode::OK,
                json,
                serde_json::json!({
                    "channels": channels,
                    "unread": unread,
                    "breach": breach,
                    "listener": pen,
                })
                .to_string(),
                format!("{text}\n"),
            )
        }
        Err(e) => error(json, StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// GET /channels/{channel}/messages — the LOG, which moves no receipt.
pub async fn channel_history(
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    // ★ A CHANNEL LOG IS NOT PUBLIC (#158 review): the store filters private memories by audience and
    // a chat log has the same problem — an incident channel is not a public page.
    if let Err(r) = proven_pen(&caller, json) {
        return r;
    }
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
        Ok(rows) => {
            let text = rows
                .iter()
                .map(|m| format!("{} {}: {}", m.seq, m.pen, m.body))
                .collect::<Vec<_>>()
                .join("\n");
            render(
                StatusCode::OK,
                json,
                serde_json::json!({ "channel": channel, "messages": rows }).to_string(),
                format!("{text}\n"),
            )
        }
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// GET /channels/breach — what, if anything, is stopping this pen from changing the record.
pub async fn channel_breach(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let Some(pen) = proven(&caller) else {
        return render(
            StatusCode::OK,
            json,
            serde_json::json!({ "breach": null, "proven": false }).to_string(),
            "none (pen not proven)\n".to_string(),
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
        Ok(found) => {
            let text = match &found {
                Some(b) => format!(
                    "#{} seq {} offered {} time(s), {}s ago (gate {})\n",
                    b.channel, b.seq, b.misses, b.age_secs, b.gate
                ),
                None => "none\n".to_string(),
            };
            render(
                StatusCode::OK,
                json,
                serde_json::json!({ "breach": found, "proven": true }).to_string(),
                text,
            )
        }
        Err(e) => error(json, StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// POST /channels/{channel}/messages — say one thing.
pub async fn channel_send(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let pen = match proven_pen(&caller, json) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        Ok(None) => return error(json, StatusCode::BAD_REQUEST, "a JSON body is required"),
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let Some(text) = doc
        .get("body")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
    else {
        return error(json, StatusCode::BAD_REQUEST, "`body` is required");
    };
    let reply_to = doc.get("reply_to").and_then(|v| v.as_i64());
    let (who, chan) = (pen.clone(), channel.clone());
    match server
        .with_store("channel_send", move |s| {
            s.channel_send(&who, &chan, &text, reply_to)
        })
        .await
    {
        Ok(msg) => render(
            StatusCode::OK,
            json,
            serde_json::to_string(&msg).unwrap_or_else(|_| "{}".into()),
            format!("sent #{} seq {}\n", msg.channel, msg.seq),
        ),
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// POST /channels/fetch — LISTEN: take the unread messages and mark them fetched.
pub async fn channel_fetch(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let pen = match proven_pen(&caller, json) {
        Ok(p) => p,
        Err(r) => return r,
    };
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
        Ok(rows) => {
            let text = rows
                .iter()
                .map(|m| format!("#{} {}: {}", m.channel, m.pen, m.body))
                .collect::<Vec<_>>()
                .join("\n");
            render(
                StatusCode::OK,
                json,
                serde_json::json!({ "fetched": rows.len(), "messages": rows }).to_string(),
                format!("{text}\n"),
            )
        }
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// POST /channels/present — a turn boundary offered this pen its inbox.
pub async fn channel_present(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let pen = match proven_pen(&caller, json) {
        Ok(p) => p,
        Err(r) => return r,
    };
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
        Ok(marked) => render(
            StatusCode::OK,
            json,
            serde_json::json!({ "pen": pen, "marked": marked }).to_string(),
            format!("presented for {pen} ({marked} unread marked)\n"),
        ),
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// POST /channels/{channel}/join — join, and `ops_team` brings `emergency` with it.
pub async fn channel_join(
    State(state): State<HttpState>,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    caller: crate::identity::Caller,
) -> Response {
    channel_membership(caller, state, channel, headers, raw, true).await
}

/// POST /channels/{channel}/leave — leave; leaving `emergency` on the team is refused.
pub async fn channel_leave(
    State(state): State<HttpState>,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    caller: crate::identity::Caller,
) -> Response {
    channel_membership(caller, state, channel, headers, raw, false).await
}

async fn channel_membership(
    caller: crate::identity::Caller,
    state: HttpState,
    channel: String,
    headers: HeaderMap,
    raw: Option<String>,
    join: bool,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let pen = match proven_pen(&caller, json) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (who, chan) = (pen.clone(), channel.clone());
    // The one remaining hand-written session guard: this helper takes the state by value, so it has
    // no `&mut Parts` for the `Server` extractor. Every handler uses the extractor instead (#233).
    let Some(server) = state.server() else {
        return error(
            json,
            StatusCode::INTERNAL_SERVER_ERROR,
            "server not attached",
        );
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
        Ok(()) => render(
            StatusCode::OK,
            json,
            serde_json::json!({ "pen": pen, "channel": channel, "member": join }).to_string(),
            format!("{} #{channel}\n", if join { "joined" } else { "left" }),
        ),
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// GET /channels/wait — block for new messages. A WAIT, not a LISTEN: no receipt moves.
pub async fn channel_wait(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let Some(pen) = proven(&caller) else {
        return error(
            json,
            StatusCode::UNAUTHORIZED,
            "refused: waiting needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`",
        );
    };
    let after = match params.maybe("after").and_then(|v| v.parse::<i64>().ok()) {
        Some(after) => after,
        None => {
            // No cursor = "from now", not the whole backlog.
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
                Err(e) => return error(json, StatusCode::INTERNAL_SERVER_ERROR, e),
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
            Err(e) => return error(json, StatusCode::INTERNAL_SERVER_ERROR, e),
        };
        if !rows.is_empty() || std::time::Instant::now() >= deadline {
            let text = rows
                .iter()
                .map(|m| format!("#{} {}: {}", m.channel, m.pen, m.body))
                .collect::<Vec<_>>()
                .join("\n");
            let truncated = rows.len() >= 200;
            return render(
                StatusCode::OK,
                json,
                serde_json::json!({ "after": after, "messages": rows, "truncated": truncated })
                    .to_string(),
                format!("{text}\n"),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }
}

/// GET /channels/{channel}/members — effective membership.
pub async fn channel_members(
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = proven_pen(&caller, json) {
        return r;
    }
    let key = channel.clone();
    match server
        .with_read(
            "channel_members",
            crate::store::audience::Reader::everything(),
            move |s| s.channel_members(&key),
        )
        .await
    {
        Ok(rows) => {
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
            render(
                StatusCode::OK,
                json,
                serde_json::json!({ "channel": channel, "members": rows }).to_string(),
                format!("{text}\n"),
            )
        }
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// POST /channels — create one. A gated channel needs `estate` or `james` (the store says so).
pub async fn channel_create(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let pen = match proven_pen(&caller, json) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => return error(json, StatusCode::BAD_REQUEST, "a JSON body is required"),
    };
    let field = |k: &str| doc.get(k).and_then(|v| v.as_str()).map(ToString::to_string);
    let (Some(channel), Some(title)) = (field("channel"), field("title")) else {
        return error(
            json,
            StatusCode::BAD_REQUEST,
            "`channel` and `title` are required",
        );
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
        Ok(row) => render(
            StatusCode::OK,
            json,
            serde_json::to_string(&row).unwrap_or_else(|_| "{}".into()),
            format!(
                "created #{} (members {}, gate {})\n",
                row.key, row.members, row.gate
            ),
        ),
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// POST /channels/{channel}/gate — change a gate; the change is a row.
pub async fn channel_set_gate(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let pen = match proven_pen(&caller, json) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => return error(json, StatusCode::BAD_REQUEST, "a JSON body is required"),
    };
    let Some(gate) = doc
        .get("gate")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
    else {
        return error(json, StatusCode::BAD_REQUEST, "`gate` is required");
    };
    let (who, chan) = (pen.clone(), channel.clone());
    match server
        .with_store("channel_set_gate", move |s| {
            s.channel_set_gate(&who, &chan, &gate)
        })
        .await
    {
        Ok(row) => render(
            StatusCode::OK,
            json,
            serde_json::to_string(&row).unwrap_or_else(|_| "{}".into()),
            format!("#{} gate is now {}\n", row.key, row.gate),
        ),
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// GET /channels/{channel}/events — who made this channel dangerous, and when.
pub async fn channel_events(
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = proven_pen(&caller, json) {
        return r;
    }
    let key = channel.clone();
    match server
        .with_read(
            "channel_events",
            crate::store::audience::Reader::everything(),
            move |s| s.channel_events(&key),
        )
        .await
    {
        Ok(rows) => {
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
            render(
                StatusCode::OK,
                json,
                serde_json::json!({ "channel": channel, "events": rows }).to_string(),
                format!("{text}\n"),
            )
        }
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// POST /channels/{channel}/retention — how long the text is kept. Keeper-only, recorded.
pub async fn channel_set_retention(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let pen = match proven_pen(&caller, json) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => return error(json, StatusCode::BAD_REQUEST, "a JSON body is required"),
    };
    let Some(keep) = doc.get("keep_secs").and_then(|v| v.as_i64()) else {
        return error(json, StatusCode::BAD_REQUEST, "`keep_secs` is required");
    };
    let (who, chan) = (pen.clone(), channel.clone());
    match server
        .with_store("channel_set_retention", move |s| {
            s.channel_set_retention(&who, &chan, keep)
        })
        .await
    {
        Ok(row) => render(
            StatusCode::OK,
            json,
            serde_json::to_string(&row).unwrap_or_else(|_| "{}".into()),
            format!(
                "#{} keeps {}\n",
                row.key,
                if row.keep_secs == 0 {
                    "forever".into()
                } else {
                    format!("{}s", row.keep_secs)
                }
            ),
        ),
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// POST /channels/prune — expire the text past retention. Receipts stay.
pub async fn channel_prune(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    // ★ THE PEN IS REQUIRED, AND THEN CONSULTED (#168 review): the gate lives on this door too.
    // The MCP tool gated `channel_prune` and this route did not, so a locked pen could destroy the
    // text it was locked out of knowing — the two doors disagreed exactly where it matters.
    let caller = match proven_pen(&caller, json) {
        Ok(p) => p,
        Err(r) => return r,
    };
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
        Ok(rows) => {
            let total: usize = rows.iter().map(|r| r.emptied).sum();
            render(
                StatusCode::OK,
                json,
                serde_json::json!({ "pruned": rows, "emptied": total }).to_string(),
                format!("emptied {total} message body(s)\n"),
            )
        }
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}

/// POST /channels/{channel}/invite — put another pen in. Keeper-only: an invitation has an inviter.
pub async fn channel_invite(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    Path(channel): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let actor = match proven_pen(&caller, json) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        _ => return error(json, StatusCode::BAD_REQUEST, "a JSON body is required"),
    };
    let Some(guest) = doc
        .get("pen")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
    else {
        return error(json, StatusCode::BAD_REQUEST, "`pen` is required");
    };
    let (who, chan, target) = (actor.clone(), channel.clone(), guest.clone());
    match server
        .with_store("channel_invite", move |s| {
            s.channel_invite(&who, &target, &chan)
        })
        .await
    {
        Ok(()) => render(
            StatusCode::OK,
            json,
            serde_json::json!({ "channel": channel, "pen": guest, "member": true, "invited_by": actor }).to_string(),
            format!("invited {guest} to #{channel}\n"),
        ),
        Err(e) => error(json, StatusCode::BAD_REQUEST, e),
    }
}
