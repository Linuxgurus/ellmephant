//! Split out of `http_surface.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` is deliberate — the shared vocabulary (the `Server` extractor, `Params`,
//! the response builders) stays in `mod.rs`, so a handler reads the same here as it did there.

use super::*;

/// `POST /mail` — write one letter (`mail_send`). JSON body; nothing is delivered.
pub async fn mail_send(
    State(state): State<HttpState>,
    server: Server,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    let scope = authorize(&state, &headers, json)?;
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        Ok(None) => return Err(ApiError::bad(json, "a JSON body is required")),
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    let field = |k: &str| doc.get(k).and_then(|v| v.as_str()).map(ToString::to_string);
    let (Some(sender), Some(recipient), Some(letter)) =
        (field("sender"), field("recipient"), field("body"))
    else {
        return Err(ApiError::bad(
            json,
            "`sender`, `recipient` and `body` are required",
        ));
    };
    let subject = field("subject").unwrap_or_default();
    let encoding = field("encoding").unwrap_or_else(|| "text".to_string());
    let to = recipient.clone();
    match server
        .with_store("mail_send", move |s| {
            s.mail_send(&sender, &sender, &recipient, &subject, &letter, &encoding)
        })
        .await
    {
        Ok(out) => {
            tracing::info!(area = scope.area, seq = out.seq, to = %to, "letter over http");
            present(
                StatusCode::OK,
                json,
                &report(
                    (out, to),
                    |(out, _)| serde_json::to_string(out).unwrap_or_default(),
                    |(out, to)| format!("letter {} written to `{to}`", out.seq),
                ),
            )
        }
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

/// `GET /mail?recipient=&since=&limit=` — envelopes after the reader's cursor (`mail_inbox`).
///
/// ★ THE CURSOR BELONGS TO THE PROVEN PEN (migration 0017). With `X-Stet-From`/`X-Stet-Auth` the
/// request reads that pen's stored cursor; an explicit `since` still wins, because a diagnostic may
/// want to look again. Anonymous is NOT an error here — reads are open — it simply has no stored
/// cursor, so `since` (or 0) decides, and nothing is written.
/// The proven pen behind an HTTP request, or the refusal to send.
#[allow(clippy::result_large_err)] // the refusal IS the axum response; boxing it would be taste, not safety.
pub(super) fn mail_reader_identity(
    caller: &crate::identity::Caller,
    json: bool,
) -> ApiResult<String> {
    caller
        .pen_or("refused: reading mail needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`")
        .map(ToString::to_string)
        .map_err(|r| refusal_response(json, r, StatusCode::FORBIDDEN))
}

/// ★ THE BOX IS THE BOUNDARY, on every door (soma/ellmephant#92).
///
/// The MCP tool has refused a non-recipient since 2026-10-18; this surface resolved an identity only
/// to find the reader's CURSOR and then served any box to anyone. One rule, one place, both doors —
/// `mail box?recipient=stet` answered 49 envelopes to an anonymous caller, measured 2026-10-22.
#[allow(clippy::result_large_err)] // same: a refusal is returned whole, as on every other handler here.
async fn mail_guard(
    state: &HttpState,
    reader: &str,
    requested: &str,
    json: bool,
    scope: crate::store::mail::BoxScope,
) -> ApiResult<()> {
    // A helper rather than a handler: no `&mut Parts`, so no extractor (#233).
    let Some(server) = state.server() else {
        return Err(ApiError::internal(json, "server not attached"));
    };
    let (r, q) = (reader.to_string(), requested.to_string());
    // ★ THE SAME RULE AS MCP, including the GRANT (soma/ellmephant#235): this asked the `read`
    // grant even when the verb was `ack`, so a pen holding only an ack grant was refused here and
    // admitted there. The sentence comes from the store, so the two doors cannot drift again.
    server
        .with_read(
            "mail_guard",
            crate::store::audience::Reader::pen(&r),
            move |s| s.mail_may_open_box(&r, &q, scope),
        )
        .await
        .map_err(|e| ApiError::store(json, e))
}

pub async fn mail_inbox(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let Some(recipient) = params.maybe("recipient").map(ToString::to_string) else {
        return Err(ApiError::new(
            json,
            StatusCode::BAD_REQUEST,
            "`recipient` is required",
        ));
    };
    let explicit = params.maybe("since").and_then(|v| v.parse::<i64>().ok());
    let limit = params
        .maybe("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_usize);
    let reader = mail_reader_identity(&caller, json)?;
    mail_guard(
        &state,
        &reader,
        &recipient,
        json,
        crate::store::mail::BoxScope::Read,
    )
    .await?;
    let me = Some(reader.clone());
    match server
        .with_read(
            "mail_inbox",
            crate::store::audience::Reader::everything(),
            move |s| {
                // The box's top, the page's top, and the READER's place are three facts (see the MCP tool).
                let stored = match &me {
                    Some(name) => s.mail_cursor(name, &recipient)?,
                    None => None,
                };
                let since = explicit.unwrap_or_else(|| stored.as_ref().map(|c| c.seq).unwrap_or(0));
                Ok((
                    s.mail_inbox(&recipient, since, limit)?,
                    s.mail_head(&recipient)?,
                    stored,
                    since,
                ))
            },
        )
        .await
    {
        Ok((rows, head, stored, since)) => {
            let page_top = rows.iter().map(|r| r.seq).max();
            let cursor = stored.as_ref().map(|c| c.seq).unwrap_or(0);
            present(
                StatusCode::OK,
                json,
                &report(
                    (stored, cursor, head, rows, page_top, since, reader),
                    |(stored, cursor, head, rows, page_top, since, reader)| {
                        serde_json::json!({
                            "reader": stored
                                .as_ref()
                                .map(|c| c.reader.clone())
                                .unwrap_or_else(|| reader.clone()),
                            "cursor": cursor,
                            "head": head,
                            "unread": (head - cursor).max(0),
                            "letters": rows,
                            "page_top": page_top,
                            "max_seq": page_top.unwrap_or(*since),
                            "newest_available": head,
                        })
                        .to_string()
                    },
                    |(_, _, _, rows, _, since, _)| {
                        if rows.is_empty() {
                            format!("no mail after {since}")
                        } else {
                            rows.iter()
                                .map(|r| {
                                    format!(
                                        "{} {} {} ({} · {} bytes)",
                                        r.seq, r.at, r.sender, r.subject, r.bytes
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                    },
                ),
            )
        }
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

/// `POST /mail/ack?recipient=&seq=` — advance the caller's own cursor (`mail_ack`).
///
/// ★ A CURSOR NEEDS A READER, so this is the one mail route that REFUSES anonymous: there is no row
/// to move without a proven pen, and guessing one would hand someone else's cursor to whoever asked.
pub async fn mail_ack(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    authorize(&state, &headers, json)?;
    let (Some(recipient), Some(seq)) = (
        params.maybe("recipient").map(ToString::to_string),
        params.maybe("seq").and_then(|v| v.parse::<i64>().ok()),
    ) else {
        return Err(ApiError::bad(json, "`recipient` and `seq` are required"));
    };
    let reader = match caller
        .pen_or("refused: a cursor belongs to a proven pen — send `X-Stet-From` and `X-Stet-Auth`")
    {
        Ok(name) => name.to_string(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::FORBIDDEN)),
    };
    mail_guard(
        &state,
        &reader,
        &recipient,
        json,
        crate::store::mail::BoxScope::Ack,
    )
    .await?;
    match server
        .with_store("mail_ack", move |s| {
            s.mail_advance(&reader, &recipient, seq)
        })
        .await
    {
        Ok(cursor) => present(
            StatusCode::OK,
            json,
            &viewed(&cursor, |cursor| {
                format!(
                    "cursor moved to {} (+{}), {} unread",
                    cursor.seq, cursor.moved, cursor.unread
                )
            }),
        ),
        Err(e) => Err(ApiError::store(json, e)),
    }
}

/// `GET /mail/{seq}` — one letter, body included (`mail_read`).
pub async fn mail_read(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    axum::extract::Path(seq): axum::extract::Path<i64>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let reader = mail_reader_identity(&caller, json)?;
    match server
        .with_read(
            "mail_read",
            crate::store::audience::Reader::everything(),
            move |s| s.mail_read(seq),
        )
        .await
    {
        Ok(Some(letter)) => {
            // The LETTER decides its box: the check is on the row, not on an argument.
            mail_guard(
                &state,
                &reader,
                &letter.recipient,
                json,
                crate::store::mail::BoxScope::Read,
            )
            .await?;
            present(
                StatusCode::OK,
                json,
                &report(
                    letter,
                    |letter| serde_json::to_string(letter).unwrap_or_default(),
                    |letter| letter.body.clone(),
                ),
            )
        }
        Ok(None) => Err(ApiError::not_found(json, "no such letter")),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

/// `POST /mail/prune?keep_days=7` — drop letters nobody ever read (`mail_prune`).
pub async fn mail_prune(
    State(state): State<HttpState>,
    server: Server,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    authorize(&state, &headers, json)?;
    let keep_days = params
        .maybe("keep_days")
        .and_then(|v| v.parse().ok())
        .unwrap_or(14_i64);
    match server
        .with_store("mail_prune", move |s| s.mail_prune(keep_days))
        .await
    {
        Ok(removed) => present(
            StatusCode::OK,
            json,
            &report(
                (removed, keep_days),
                |(removed, _)| serde_json::json!({ "removed": removed }).to_string(),
                |(removed, keep_days)| {
                    format!("pruned {removed} letter(s) older than {keep_days}d")
                },
            ),
        ),
        Err(e) => Err(ApiError::bad(json, e)),
    }
}

// ── MAIL SURROGATES (#153) ───────────────────────────────────────────────────────────────────────

/// `POST /mail/surrogate/grant` — lend your OWN name for mail.
pub async fn mail_surrogate_grant(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let caller = match caller.pen_or(
        "refused: granting a name needs a proven pen — send `X-Stet-From` and `X-Stet-Auth`",
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
        grantor: String,
        surrogate: String,
        #[serde(default)]
        scopes: Option<String>,
        #[serde(default)]
        expires_at: Option<String>,
        #[serde(default)]
        note: Option<String>,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return Err(ApiError::bad(json, format!("bad JSON: {e}"))),
    };
    if !caller.eq_ignore_ascii_case(ask.grantor.trim()) {
        return Err(ApiError::new(
            json,
            StatusCode::FORBIDDEN,
            format!(
                "refused: `{caller}` may grant its OWN name only; `{}` must grant itself",
                ask.grantor
            ),
        ));
    }
    let scopes = ask.scopes.unwrap_or_else(|| "send".to_string());
    match server
        .with_store("mail_surrogate_grant", move |s| {
            s.mail_grant(
                &ask.grantor,
                &ask.surrogate,
                &scopes,
                ask.expires_at.as_deref(),
                ask.note.as_deref(),
            )
        })
        .await
    {
        Ok(grant) => present(
            StatusCode::CREATED,
            json,
            &report(
                grant,
                |grant| {
                    serde_json::to_value(grant)
                        .unwrap_or(serde_json::Value::Null)
                        .to_string()
                },
                |_| "granted\n".to_string(),
            ),
        ),
        Err(e) => Err(ApiError::bad(json, &e)),
    }
}

/// `POST /mail/surrogate/revoke` — take a lent name back.
pub async fn mail_surrogate_revoke(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
    body: Body,
) -> ApiResult<Response> {
    let json = Params::parse(raw.as_deref()).json();
    authorize(&state, &headers, json)?;
    let caller = match caller.pen_or("refused: revoking needs a proven pen") {
        Ok(name) => name.to_string(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    let bytes = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return Err(ApiError::bad(json, e)),
    };
    #[derive(serde::Deserialize)]
    struct Ask {
        grantor: String,
        surrogate: String,
    }
    let ask: Ask = match serde_json::from_slice(&bytes) {
        Ok(a) => a,
        Err(e) => return Err(ApiError::bad(json, format!("bad JSON: {e}"))),
    };
    if !caller.eq_ignore_ascii_case(ask.grantor.trim()) {
        return Err(ApiError::new(
            json,
            StatusCode::FORBIDDEN,
            format!("refused: `{caller}` may revoke its OWN grants only"),
        ));
    }
    match server
        .with_store("mail_surrogate_revoke", move |s| {
            s.mail_revoke(&ask.grantor, &ask.surrogate)
        })
        .await
    {
        Ok(n) => present(
            StatusCode::OK,
            json,
            &report(
                n,
                |n| serde_json::json!({ "revoked": n }).to_string(),
                |n| format!("revoked {n}\n"),
            ),
        ),
        Err(e) => Err(ApiError::bad(json, &e)),
    }
}

/// `GET /mail/surrogate` — every grant this pen holds, live or not.
pub async fn mail_surrogate_list(
    State(state): State<HttpState>,
    server: Server,
    caller: crate::identity::Caller,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    authorize(&state, &headers, json)?;
    let caller = match caller.pen_or("refused: listing grants needs a proven pen") {
        Ok(name) => name.to_string(),
        Err(r) => return Err(refusal_response(json, r, StatusCode::UNAUTHORIZED)),
    };
    let subject = params
        .maybe("surrogate")
        .map(ToString::to_string)
        .unwrap_or(caller);
    match server
        .with_read(
            "mail_surrogate_list",
            crate::store::audience::Reader::pen(&subject),
            move |s| s.mail_surrogates_of(&subject),
        )
        .await
    {
        Ok(rows) => present(
            StatusCode::OK,
            json,
            &report(
                rows,
                |rows| {
                    serde_json::to_value(rows)
                        .unwrap_or(serde_json::Value::Null)
                        .to_string()
                },
                |rows| format!("{}\n", rows.len()),
            ),
        ),
        Err(e) => Err(ApiError::bad(json, &e)),
    }
}

// ── ELLMECHAT (#151): the channels ─────────────────────────────────────────────────────────────
//
// Thin on purpose. Every rule — membership, fan-out, the three-event ledger, the gate — lives in
// `store::channel`, so this door cannot disagree with the MCP door or the store.
//
// A READ needs no credential (reads are never gated, and reading is the way out of a lock).
// A WRITE needs a bearer AND a proven pen: the pen is who is speaking, the bearer says this client
// may speak at all.
