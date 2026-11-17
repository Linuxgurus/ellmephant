//! Split out of `http_surface.rs` (soma/ellmephant#50): mechanical, no behaviour change.
//!
//! `use super::*` is deliberate — the shared vocabulary (the `Server` extractor, `Params`,
//! the response builders) stays in `mod.rs`, so a handler reads the same here as it did there.

/// One MCP tool reachable over HTTP — the parity table. A route the table does not name is a tool
/// a wedged MCP session would strand; the parity test keeps it exactly as long as `TOOLS`.
pub struct Route {
    /// HTTP method.
    pub method: &'static str,
    /// Path template (`{key}` is the axum 0.8 placeholder).
    pub path: &'static str,
    /// The MCP tool this route mirrors.
    pub tool: &'static str,
}

/// Every MCP tool, and the route that reaches the same `Session` call (`/health` and `/bootstrap`
/// are surfaces of their own, not tools, so they are absent by design).
pub const ROUTES: &[Route] = &[
    Route {
        method: "GET",
        path: "/whoami",
        tool: "whoami",
    },
    Route {
        method: "GET",
        path: "/whois/{pen}",
        tool: "whois",
    },
    Route {
        method: "POST",
        path: "/pen/mint",
        tool: "pen_mint",
    },
    Route {
        method: "POST",
        path: "/pen/rotate",
        tool: "pen_rotate",
    },
    Route {
        method: "POST",
        path: "/pen/lend",
        tool: "pen_lend",
    },
    Route {
        method: "POST",
        path: "/sudo",
        tool: "sudo",
    },
    Route {
        method: "POST",
        path: "/sudo/pass",
        tool: "sudo_pass",
    },
    Route {
        method: "POST",
        path: "/sudo/release",
        tool: "sudo_release",
    },
    Route {
        method: "GET",
        path: "/sudo/sessions",
        tool: "sudo_sessions",
    },
    Route {
        method: "POST",
        path: "/memory",
        tool: "memory_upsert",
    },
    Route {
        method: "DELETE",
        path: "/memory/{key}",
        tool: "memory_delete",
    },
    Route {
        method: "DELETE",
        path: "/history",
        tool: "memory_history_erase",
    },
    Route {
        method: "GET",
        path: "/history/search",
        tool: "memory_history_search",
    },
    Route {
        method: "GET",
        path: "/unowned",
        tool: "memory_unowned",
    },
    Route {
        method: "POST",
        path: "/memory/{key}/merge",
        tool: "memory_merge",
    },
    Route {
        method: "POST",
        path: "/memory/{key}/append",
        tool: "memory_append",
    },
    Route {
        method: "POST",
        path: "/memory/{key}/fold",
        tool: "memory_fold",
    },
    Route {
        method: "POST",
        path: "/memory/{key}/adopt",
        tool: "memory_adopt",
    },
    Route {
        method: "GET",
        path: "/search",
        tool: "memory_search",
    },
    Route {
        method: "GET",
        path: "/semantic",
        tool: "semantic_search",
    },
    Route {
        method: "GET",
        path: "/memory/{key}",
        tool: "memory_get",
    },
    Route {
        method: "GET",
        path: "/history",
        tool: "memory_history",
    },
    Route {
        method: "GET",
        path: "/memory/{key}/backlinks",
        tool: "memory_backlinks",
    },
    Route {
        method: "GET",
        path: "/dangling",
        tool: "memory_dangling",
    },
    Route {
        method: "POST",
        path: "/memory/{key}/upvote",
        tool: "memory_upvote",
    },
    Route {
        method: "GET",
        path: "/stats",
        tool: "memory_stats",
    },
    Route {
        method: "GET",
        path: "/dupes",
        tool: "memory_dupes",
    },
    Route {
        method: "POST",
        path: "/reembed",
        tool: "memory_reembed",
    },
    Route {
        method: "POST",
        path: "/reparse",
        tool: "memory_reparse",
    },
    Route {
        method: "POST",
        path: "/project",
        tool: "project_upsert",
    },
    Route {
        method: "GET",
        path: "/project/{key}",
        tool: "project_get",
    },
    Route {
        method: "GET",
        path: "/projects",
        tool: "project_list",
    },
    Route {
        method: "GET",
        path: "/config",
        tool: "config_list",
    },
    Route {
        method: "POST",
        path: "/config",
        tool: "config_define",
    },
    Route {
        method: "GET",
        path: "/config/{key}",
        tool: "config_get",
    },
    Route {
        method: "POST",
        path: "/config/{key}",
        tool: "config_set",
    },
    Route {
        method: "POST",
        path: "/claim",
        tool: "claim_acquire",
    },
    Route {
        method: "POST",
        path: "/claim/{key}/release",
        tool: "claim_release",
    },
    Route {
        method: "GET",
        path: "/claim",
        tool: "claim_list",
    },
    Route {
        method: "POST",
        path: "/mail",
        tool: "mail_send",
    },
    Route {
        method: "GET",
        path: "/mail",
        tool: "mail_inbox",
    },
    Route {
        method: "GET",
        path: "/mail/{seq}",
        tool: "mail_read",
    },
    Route {
        method: "POST",
        path: "/mail/ack",
        tool: "mail_ack",
    },
    Route {
        method: "POST",
        path: "/mail/prune",
        tool: "mail_prune",
    },
    Route {
        method: "POST",
        path: "/mail/surrogate/grant",
        tool: "mail_surrogate_grant",
    },
    Route {
        method: "POST",
        path: "/mail/surrogate/revoke",
        tool: "mail_surrogate_revoke",
    },
    Route {
        method: "GET",
        path: "/mail/surrogate",
        tool: "mail_surrogate_list",
    },
    Route {
        method: "GET",
        path: "/channels",
        tool: "channel_list",
    },
    Route {
        method: "POST",
        path: "/channels/{channel}/messages",
        tool: "channel_send",
    },
    Route {
        method: "GET",
        path: "/channels/{channel}/messages",
        tool: "channel_history",
    },
    Route {
        method: "POST",
        path: "/channels/fetch",
        tool: "channel_fetch",
    },
    Route {
        method: "POST",
        path: "/channels/present",
        tool: "channel_present",
    },
    Route {
        method: "GET",
        path: "/channels/breach",
        tool: "channel_breach",
    },
    Route {
        method: "POST",
        path: "/channels/{channel}/join",
        tool: "channel_join",
    },
    Route {
        method: "POST",
        path: "/channels/{channel}/leave",
        tool: "channel_leave",
    },
    Route {
        method: "GET",
        path: "/channels/wait",
        tool: "channel_wait",
    },
    Route {
        method: "GET",
        path: "/channels/{channel}/members",
        tool: "channel_members",
    },
    Route {
        method: "POST",
        path: "/channels",
        tool: "channel_create",
    },
    Route {
        method: "POST",
        path: "/channels/{channel}/gate",
        tool: "channel_set_gate",
    },
    Route {
        method: "GET",
        path: "/channels/{channel}/events",
        tool: "channel_events",
    },
    Route {
        method: "POST",
        path: "/channels/{channel}/retention",
        tool: "channel_set_retention",
    },
    Route {
        method: "POST",
        path: "/channels/prune",
        tool: "channel_prune",
    },
    Route {
        method: "POST",
        path: "/channels/{channel}/invite",
        tool: "channel_invite",
    },
];
