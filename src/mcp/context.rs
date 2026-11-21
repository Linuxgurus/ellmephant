use super::*;

pub(super) fn caller_of(
    ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
    declared: Option<&str>,
    paths: &Paths,
) -> crate::identity::Caller {
    match ctx.extensions.get::<axum::http::request::Parts>() {
        Some(parts) => {
            crate::identity::Caller::from_headers(paths, &parts.headers).declare(declared)
        }
        None => crate::identity::Caller::transport(declared),
    }
}

pub(super) fn reader_of(
    ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
    declared: Option<&str>,
    paths: &Paths,
) -> crate::store::audience::Reader {
    caller_of(ctx, declared, paths).reader()
}

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
