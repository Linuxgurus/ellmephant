//! Split out of `http_surface.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` is deliberate — the shared vocabulary (the `Server` extractor, `Params`,
//! the response builders) stays in `mod.rs`, so a handler reads the same here as it did there.

use super::*;

/// `POST /pen/lend` — lend this request's pen's authority down to a descendant (`pen_lend`).
pub async fn pen_lend(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let caller = match caller
        .pen_or("refused: lending needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`")
    {
        Ok(name) => name.to_string(),
        Err(r) => return refusal_response(json, r, StatusCode::UNAUTHORIZED),
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        pen: String,
        secs: i64,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, format!("bad JSON: {e}")),
    };
    match server
        .with_store("pen_lend", move |s| s.lend(&caller, &ask.pen, ask.secs))
        .await
    {
        Ok(session) => {
            tracing::info!(
                area = scope.area,
                weak = %session.weak,
                powerful = %session.powerful,
                "authority lent down over http"
            );
            let value = serde_json::to_value(&session).unwrap_or(serde_json::Value::Null);
            render(
                StatusCode::CREATED,
                json,
                value.to_string(),
                format!(
                    "lent {} -> {} until {}\n",
                    session.powerful, session.weak, session.expires_at
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /sudo` — borrow a pen's authority (`sudo`).
pub async fn sudo(
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
    let caller = match proven_pen(&caller, json) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        powerful: String,
        secs: i64,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, format!("bad JSON: {e}")),
    };
    match server
        .with_store("sudo", move |s| s.sudo(&caller, &ask.powerful, ask.secs))
        .await
    {
        Ok(session) => {
            let value = serde_json::to_value(&session).unwrap_or(serde_json::Value::Null);
            render(
                StatusCode::CREATED,
                json,
                value.to_string(),
                format!(
                    "borrowed {} as {} until {}\n",
                    session.powerful, session.weak, session.expires_at
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /sudo/sessions` — who holds what, until when (`sudo_sessions`).
///
/// Read-only, and the answer to "has my authority lapsed?" without a guess. Any proven pen may ask:
/// sessions are the estate's business — an agent that is about to act on borrowed authority should be
/// able to see its own clock, and an operator should be able to see everyone's.
pub async fn sudo_sessions(
    State(state): State<HttpState>,
    server: Server,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    if let Err(r) = authorize(&state, &headers, json) {
        return r;
    }
    let params = Params::parse(raw.as_deref());
    let pen = params.get("pen").map(ToString::to_string);
    match server
        .with_store("sudo_sessions", move |s| s.sudo_sessions(pen.as_deref()))
        .await
    {
        Ok(sessions) => {
            let value = serde_json::to_value(&sessions).unwrap_or(serde_json::Value::Null);
            let text = if sessions.is_empty() {
                "no live sessions\n".to_string()
            } else {
                sessions
                    .iter()
                    .map(|s| {
                        format!(
                            "{} holds {} ({}) until {}\n",
                            s.weak, s.powerful, s.source, s.expires_at
                        )
                    })
                    .collect()
            };
            render(StatusCode::OK, json, value.to_string(), text)
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /sudo/pass` — pass a session down (`sudo_pass`).
pub async fn sudo_pass(
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
    let caller = match proven_pen(&caller, json) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        powerful: String,
        pen: String,
        secs: i64,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, format!("bad JSON: {e}")),
    };
    match server
        .with_store("sudo_pass", move |s| {
            s.sudo_pass(&caller, &ask.powerful, &ask.pen, ask.secs)
        })
        .await
    {
        Ok(session) => {
            let value = serde_json::to_value(&session).unwrap_or(serde_json::Value::Null);
            render(
                StatusCode::CREATED,
                json,
                value.to_string(),
                format!(
                    "passed {} to {} until {}\n",
                    session.powerful, session.weak, session.expires_at
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /sudo/release` — end a session and its chain (`sudo_release`).
pub async fn sudo_release(
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
    let caller = match proven_pen(&caller, json) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        powerful: String,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, format!("bad JSON: {e}")),
    };
    match server
        .with_store("sudo_release", move |s| {
            s.sudo_release(&caller, &ask.powerful)
        })
        .await
    {
        Ok(killed) => render(
            StatusCode::OK,
            json,
            serde_json::json!({ "status": "released", "sessions": killed }).to_string(),
            format!("released: {killed} session(s)\n"),
        ),
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /pen/mint` — create a pen under this request's pen (`pen_mint`).
pub async fn pen_mint(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    // A mint is a WRITE: the caller is the parent, and an unproven caller is nobody.
    let caller = match caller
        .pen_or("refused: minting needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`")
    {
        Ok(name) => name.to_string(),
        Err(r) => return refusal_response(json, r, StatusCode::UNAUTHORIZED),
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        name: String,
        kind: String,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, format!("bad JSON: {e}")),
    };
    match server
        .with_store("pen_mint", move |s| s.mint(&caller, &ask.name, &ask.kind))
        .await
    {
        Ok(minted) => {
            tracing::info!(
                area = scope.area,
                pen = %minted.name,
                parent = %minted.parent,
                "pen minted over http"
            );
            let value = serde_json::to_value(&minted).unwrap_or(serde_json::Value::Null);
            render(
                StatusCode::CREATED,
                json,
                value.to_string(),
                format!(
                    "minted {} ({}) under {}\nsecret (shown once): {}\n",
                    minted.name, minted.kind, minted.parent, minted.secret
                ),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /pen/rotate` — replace a credential, returning the new secret once (`pen_rotate`).
pub async fn pen_rotate(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let scope = match authorize(&state, &headers, json) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let caller = match caller.pen_or(
        "refused: rotating a credential needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`",
    ) {
        Ok(name) => name.to_string(),
        Err(r) => return refusal_response(json, r, StatusCode::UNAUTHORIZED),
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        pen: Option<String>,
    }
    // An empty body is the common case: "rotate MY secret". It is not a malformed request.
    let ask: Ask = if bytes.iter().all(u8::is_ascii_whitespace) {
        Ask { pen: None }
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(a) => a,
            Err(e) => return error(json, StatusCode::BAD_REQUEST, format!("bad JSON: {e}")),
        }
    };
    let target = ask.pen.unwrap_or_else(|| caller.clone());
    let for_store = target.clone();
    match server
        .with_store("pen_rotate", move |s| {
            s.rotate_credential(&caller, &for_store)
        })
        .await
    {
        Ok(secret) => {
            tracing::info!(area = scope.area, pen = %target, "credential rotated over http");
            render(
                StatusCode::OK,
                json,
                serde_json::json!({ "pen": target, "secret": secret }).to_string(),
                format!("rotated {target}; new secret (shown once): {secret}\n"),
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /whois/{pen}` — one pen and what it is to this request's pen (`whois`).
pub async fn whois(
    server: Server,
    caller: crate::identity::Caller,
    Path(pen): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let json = Params::parse(raw.as_deref()).json();
    let viewer = caller
        .proven()
        .map(ToString::to_string)
        .unwrap_or_else(|| crate::identity::ANONYMOUS.to_string());
    match server
        .with_read("whois", reader_from(&caller), move |s| {
            s.whois(&viewer, &pen)
        })
        .await
    {
        Ok(facts) => {
            let value = serde_json::to_value(&facts).unwrap_or(serde_json::Value::Null);
            let text = format!(
                "{}\n  {:<10}{}\n  {:<10}{}\n  {:<10}{}\n  {:<10}{}\n  {:<10}{} row(s)\n  {:<10}{}\n",
                facts.name,
                "kind",
                facts.kind,
                "parent",
                facts.parent.as_deref().unwrap_or("(root)"),
                "created",
                facts.created,
                "state",
                facts
                    .ceased
                    .as_deref()
                    .map(|c| format!("retired {c}"))
                    .unwrap_or_else(|| "live".to_string()),
                "owns",
                facts.owns,
                "relation",
                facts.relation.as_deref().unwrap_or("(no common root)"),
            );
            render(StatusCode::OK, json, value.to_string(), text)
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /whoami` — the identity this request would be stamped with, and whether it is PROVEN.
/// Touches no store and changes nothing: the REST twin of the MCP `whoami`, so a `curl` can ask the
/// same question the model asks instead of guessing from a remembered flag.
pub async fn whoami(caller: crate::identity::Caller) -> Response {
    let (doc, text) = match caller.denied() {
        None => match caller.proven() {
            Some(name) => {
                let text = format!("{name} (proven)\n");
                (
                    serde_json::json!({ "asserted_by": name.as_str(), "proven": true }),
                    text,
                )
            }
            None => (
                serde_json::json!({ "asserted_by": crate::identity::ANONYMOUS, "proven": false }),
                format!("{} (not proven)\n", crate::identity::ANONYMOUS),
            ),
        },
        // ★ A FORGED CLAIM IS STILL WORTH REPORTING; AN UNREADABLE STORE IS NOT AN ANSWER
        // (2026-10-30, Stet on #204). Forged stays a 200 that names the claim, because that is what
        // `whoami` is for; Unavailable is a 503, because "anonymous" would be a guess presented as a
        // fact.
        Some(crate::identity::Denied::Unavailable(why)) => {
            return error(
                true,
                StatusCode::SERVICE_UNAVAILABLE,
                format!("authentication is unavailable: {why}"),
            );
        }
        Some(crate::identity::Denied::Forged(claimed)) => {
            let mut doc = serde_json::json!({
                "asserted_by": crate::identity::ANONYMOUS,
                "proven": false,
            });
            doc["claimed"] = serde_json::json!(claimed);
            doc["problem"] = serde_json::json!("the X-Stet-Auth proof is missing or wrong");
            let text = format!(
                "{} (not proven; claimed `{claimed}`)\n",
                crate::identity::ANONYMOUS
            );
            (doc, text)
        }
    };
    render(StatusCode::OK, true, doc.to_string(), text)
}

/// `GET /stats` — the whole-store digest (`memory_stats`).
pub async fn stats(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> Response {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let with_tags = params.flag("tags");
    match server
        .with_read("memory_stats", reader, move |s| s.stats(with_tags))
        .await
    {
        Ok(stats) => render(
            StatusCode::OK,
            json,
            serde_json::to_string(&stats).unwrap_or_default(),
            stats_text(&stats),
        ),
        Err(e) => error(json, error_status(&e), e),
    }
}
