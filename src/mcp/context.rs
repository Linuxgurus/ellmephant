//! Split out of `mcp/mod.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` keeps the vocabulary a tool method already reads (the `MemoryServer`, the
//! surface split, the descriptions) in scope.

use super::*;

/// The caller's PROVEN identity, read from the HTTP request that carried this call.
///
/// `Ok(None)` means nothing was claimed — a stdio transport has no HTTP request, and a client that
/// sends no headers has claimed nothing. `Err` means a name was claimed and could not be proved, and
/// it is REFUSED rather than downgraded to `anonymous`: an unproven name is a lie with a signature.
/// The caller's identity, built once per tool call (soma/ellmephant#232).
///
/// A transport with no HTTP request (stdio, in-process) may declare its pen at construction: it is
/// as trusted as the process that started it. An HTTP request's headers decide, and a FORGED claim
/// is kept as a refusal rather than downgraded to `anonymous` — an unproven name is a lie with a
/// signature.
pub(super) fn caller_of(
    ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
    declared: Option<&str>,
    paths: &Paths,
) -> crate::identity::Caller {
    match ctx.extensions.get::<axum::http::request::Parts>() {
        // Headers decide. A declaration stands in only when nothing was claimed at all — a forged
        // claim is a refusal, never a fallback (that is the difference `Caller::proven` keeps).
        Some(parts) => {
            crate::identity::Caller::from_headers(paths, &parts.headers).declare(declared)
        }
        None => crate::identity::Caller::transport(declared),
    }
}

/// Who this READ is for. A proven pen filters; a transport that declared its pen once is as trusted
/// as its parent process; anything else — including a FORGED claim — is nobody, and nobody sees only
/// public rows. A refusal is not an error here: reads stay open, they just stay narrow.
pub(super) fn reader_of(
    ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
    declared: Option<&str>,
    paths: &Paths,
) -> crate::store::audience::Reader {
    caller_of(ctx, declared, paths).reader()
}

/// ★ A WRITE REQUIRES A PEN. Reads are open to anyone; a write with NO proven identity is REFUSED,
/// not stamped `anonymous`. Anonymous is an honest answer to "who spoke", but the store has no
/// reason to accept an unsigned hand: an anonymous write is indistinguishable from one nobody meant,
/// and it cannot be held to anything. Every refusal costs one header pair to fix; `whoami` says what
/// this connection currently is.
pub(super) fn pen_name(
    ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
    declared: Option<&str>,
    paths: &Paths,
) -> std::result::Result<String, String> {
    caller_of(ctx, declared, paths)
        .pen_or(
            "refused: a write requires a proven identity — send `X-Stet-From` and `X-Stet-Auth` (reads need neither). `whoami` reports what this connection is.",
        )
        .map(ToString::to_string)
        .map_err(|r| r.to_string())
}
