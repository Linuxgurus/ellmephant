//! Split out of `http_surface.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` is deliberate — the shared vocabulary (the `Server` extractor, `Params`,
//! the response builders) stays in `mod.rs`, so a handler reads the same here as it did there.

use super::*;

/// `POST /claim` — take or renew an atomic lease (`claim_acquire`); JSON body of the tool's args.
pub async fn claim_acquire(
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
    let doc = match json_body(&headers, body).await {
        Ok(Some(v)) => v,
        Ok(None) => return error(json, StatusCode::BAD_REQUEST, "a JSON body is required"),
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let name = match doc.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return error(json, StatusCode::BAD_REQUEST, "missing `name`"),
    };
    let owner = match doc.get("owner").and_then(|v| v.as_str()) {
        Some(o) => o.to_string(),
        None => return error(json, StatusCode::BAD_REQUEST, "missing `owner`"),
    };
    let ttl = doc.get("ttl_seconds").and_then(|v| v.as_i64());
    match server
        .with_store("claim_acquire", move |s| {
            s.claim_acquire(&name, &owner, ttl)
        })
        .await
    {
        Ok(out) => {
            tracing::info!(
                area = scope.area,
                name = %out.claim.name,
                owner = %out.claim.owner,
                won = out.won,
                "claim over http"
            );
            let text = if out.won {
                format!(
                    "claimed `{}` for {} until {}",
                    out.claim.name, out.claim.owner, out.claim.expires_at
                )
            } else {
                format!(
                    "refused: `{}` held by {} until {}",
                    out.claim.name, out.claim.owner, out.claim.expires_at
                )
            };
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&out).unwrap_or_default(),
                text,
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `POST /claim/{key}/release` — drop your own lease (`claim_release`). `owner` from body or query.
pub async fn claim_release(
    State(state): State<HttpState>,
    server: Server,
    Path(name): Path<String>,
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
    let doc = match json_body(&headers, body).await {
        Ok(v) => v,
        Err(e) => return error(json, StatusCode::BAD_REQUEST, e),
    };
    let owner = doc
        .as_ref()
        .and_then(|v| v.get("owner"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .or_else(|| params.maybe("owner").map(ToString::to_string));
    let Some(owner) = owner else {
        return error(json, StatusCode::BAD_REQUEST, "missing `owner`");
    };
    // ★ THE HTTP SURFACE HAS NO PEN, ONLY A STORE-WIDE TOKEN, so it cannot check ancestry. It keeps
    // the owner check and says so: an ancestor release is an MCP act, where the caller is proven.
    match server
        .with_store("claim_release", move |s| {
            s.claim_release(&name, &owner, None)
        })
        .await
    {
        Ok(out) => {
            tracing::info!(
                area = scope.area,
                released = out.released,
                "claim release over http"
            );
            let text = if out.released {
                "released".to_string()
            } else {
                match &out.claim {
                    Some(c) => format!("refused: held by {} until {}", c.owner, c.expires_at),
                    None => "refused: no such claim".to_string(),
                }
            };
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&out).unwrap_or_default(),
                text,
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}

/// `GET /claim` — every claim, live or expired (`claim_list`).
pub async fn claims(
    State(_state): State<HttpState>,
    server: Server,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = Params::parse(raw.as_deref());
    let json = params.json();
    let name = params.maybe("name").map(ToString::to_string);
    match server
        .with_read(
            "claim_list",
            crate::store::audience::Reader::everything(),
            move |s| s.claims(name.as_deref()),
        )
        .await
    {
        Ok(rows) => {
            let text = if rows.is_empty() {
                "no claims".to_string()
            } else {
                rows.iter()
                    .map(|c| {
                        format!(
                            "{} {} held by {} (until {})",
                            if c.live { "live" } else { "expired" },
                            c.name,
                            c.owner,
                            c.expires_at
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            render(
                StatusCode::OK,
                json,
                serde_json::to_string(&serde_json::json!({ "claims": rows })).unwrap_or_default(),
                text,
            )
        }
        Err(e) => error(json, error_status(&e), e),
    }
}
