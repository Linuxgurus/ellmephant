use super::*;

pub async fn pen_lend(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let caller = match caller
        .pen_or("refused: lending needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`")
    {
        Ok(name) => name.to_string(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        pen: String,
        secs: i64,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return Err(ApiError::bad(json, format!("bad JSON: {e}"))),
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
            present(
                StatusCode::CREATED,
                json,
                &viewed(&session, |s| {
                    format!("lent {} -> {} until {}\n", s.powerful, s.weak, s.expires_at)
                }),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn sudo(
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
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        powerful: String,
        secs: i64,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return Err(ApiError::bad(json, format!("bad JSON: {e}"))),
    };
    match server
        .with_store("sudo", move |s| s.sudo(&caller, &ask.powerful, ask.secs))
        .await
    {
        Ok(session) => present(
            StatusCode::CREATED,
            json,
            &viewed(&session, |s| {
                format!(
                    "borrowed {} as {} until {}\n",
                    s.powerful, s.weak, s.expires_at
                )
            }),
        ),
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn sudo_sessions(
    State(state): State<HttpState>,
    server: Server,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let params = Params::parse(raw.as_deref());
    let pen = params.get("pen").map(ToString::to_string);
    match server
        .with_store("sudo_sessions", move |s| s.sudo_sessions(pen.as_deref()))
        .await
    {
        Ok(sessions) => present(
            StatusCode::OK,
            json,
            &report(
                sessions,
                |sessions| {
                    serde_json::to_value(sessions)
                        .unwrap_or(serde_json::Value::Null)
                        .to_string()
                },
                |sessions| {
                    if sessions.is_empty() {
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
                    }
                },
            ),
        ),
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn sudo_pass(
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
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        powerful: String,
        pen: String,
        secs: i64,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return Err(ApiError::bad(json, format!("bad JSON: {e}"))),
    };
    match server
        .with_store("sudo_pass", move |s| {
            s.sudo_pass(&caller, &ask.powerful, &ask.pen, ask.secs)
        })
        .await
    {
        Ok(session) => present(
            StatusCode::CREATED,
            json,
            &viewed(&session, |s| {
                format!(
                    "passed {} to {} until {}\n",
                    s.powerful, s.weak, s.expires_at
                )
            }),
        ),
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn sudo_release(
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
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        powerful: String,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return Err(ApiError::bad(json, format!("bad JSON: {e}"))),
    };
    match server
        .with_store("sudo_release", move |s| {
            s.sudo_release(&caller, &ask.powerful)
        })
        .await
    {
        Ok(killed) => present(
            StatusCode::OK,
            json,
            &report(
                killed,
                |killed| {
                    serde_json::json!({ "status": "released", "sessions": killed }).to_string()
                },
                |killed| format!("released: {killed} session(s)\n"),
            ),
        ),
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn pen_mint(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let caller = match caller
        .pen_or("refused: minting needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`")
    {
        Ok(name) => name.to_string(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        name: String,
        kind: String,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return Err(ApiError::bad(json, format!("bad JSON: {e}"))),
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
            present(
                StatusCode::CREATED,
                json,
                &viewed(&minted, |m| {
                    format!(
                        "minted {} ({}) under {}\nsecret (shown once): {}\n",
                        m.name, m.kind, m.parent, m.secret
                    )
                }),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn pen_rotate(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let caller = match caller.pen_or(
        "refused: rotating a credential needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`",
    ) {
        Ok(name) => name.to_string(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        pen: Option<String>,
    }
    let ask: Ask = if bytes.iter().all(u8::is_ascii_whitespace) {
        Ask { pen: None }
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(a) => a,
            Err(e) => return Err(ApiError::bad(json, format!("bad JSON: {e}"))),
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
            present(
                StatusCode::OK,
                json,
                &report(
                    (target, secret),
                    |(target, secret)| {
                        serde_json::json!({ "pen": target, "secret": secret }).to_string()
                    },
                    |(target, secret)| {
                        format!("rotated {target}; new secret (shown once): {secret}\n")
                    },
                ),
            )
        }
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn whois(
    server: Server,
    caller: crate::identity::Caller,
    Path(pen): Path<String>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
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
        Ok(facts) => present(
            StatusCode::OK,
            json,
            &report(
                facts,
                |facts| {
                    serde_json::to_value(facts)
                        .unwrap_or(serde_json::Value::Null)
                        .to_string()
                },
                |facts| {
                    format!(
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
                    )
                },
            ),
        ),
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn whoami(caller: crate::identity::Caller) -> ApiResult<Response> {
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
        Some(crate::identity::Denied::Unavailable(why)) => {
            return Err(ApiError::new(
                true,
                StatusCode::SERVICE_UNAVAILABLE,
                format!("authentication is unavailable: {why}"),
            ));
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
    present(
        StatusCode::OK,
        true,
        &report(
            (doc, text),
            |(doc, _)| doc.to_string(),
            |(_, text)| text.clone(),
        ),
    )
}

pub async fn memory_health(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    match server
        .with_read("memory_health", reader_from(&caller), |s| s.health())
        .await
    {
        Ok(health) => present(StatusCode::OK, json, &viewed(&health, health_text)),
        Err(e) => Err(ApiError::store(json, e)),
    }
}

pub async fn stats(
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let reader = reader_from(&caller);
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let with_tags = params.flag("tags");
    match server
        .with_read("memory_stats", reader, move |s| s.stats(with_tags))
        .await
    {
        Ok(stats) => present(StatusCode::OK, json, &viewed(&stats, stats_text)),
        Err(e) => Err(ApiError::store(json, e)),
    }
}
