//! The one table: every MCP tool, its REST group, and whether it is on the core surface.
//!
//! A tool name used to live in four places (`TOOLS`, `CORE_TOOLS`, the core-surface filter
//! and `group_of`), kept in agreement by a test whose whole job was to catch them
//! disagreeing. Membership is a fact about a tool, so it is stated once, per tool.

/// One row of [`TOOLS`]. `group` is the REST shelf (`/api/<group>`); `core` puts the tool on
/// the `/mcp/core` hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub group: &'static str,
    pub core: bool,
}

impl ToolSpec {
    const fn new(name: &'static str, group: &'static str, core: bool) -> Self {
        Self { name, group, core }
    }
}

/// Source order is the order the core surface advertises, so keep rows where they are.
pub const TOOLS: &[ToolSpec] = &[
    ToolSpec::new("whoami", "core", true),
    ToolSpec::new("memory_unowned", "hygiene", false),
    ToolSpec::new("whois", "identity", false),
    ToolSpec::new("pen_mint", "identity", false),
    ToolSpec::new("pen_rotate", "identity", false),
    ToolSpec::new("pen_lend", "identity", false),
    ToolSpec::new("sudo", "identity", false),
    ToolSpec::new("sudo_pass", "identity", false),
    ToolSpec::new("sudo_release", "identity", false),
    ToolSpec::new("sudo_sessions", "identity", false),
    ToolSpec::new("memory_upsert", "core", true),
    ToolSpec::new("memory_append", "hygiene", false),
    ToolSpec::new("memory_delete", "core", true),
    ToolSpec::new("memory_history_erase", "hygiene", false),
    ToolSpec::new("memory_history_search", "hygiene", false),
    ToolSpec::new("memory_merge", "hygiene", false),
    ToolSpec::new("memory_fold", "hygiene", false),
    ToolSpec::new("memory_adopt", "hygiene", false),
    ToolSpec::new("memory_search", "core", true),
    ToolSpec::new("semantic_search", "hygiene", false),
    ToolSpec::new("memory_get", "core", true),
    ToolSpec::new("memory_history", "core", true),
    ToolSpec::new("memory_backlinks", "core", true),
    ToolSpec::new("memory_dangling", "hygiene", false),
    ToolSpec::new("memory_upvote", "core", true),
    ToolSpec::new("memory_stats", "core", true),
    ToolSpec::new("memory_health", "hygiene", false),
    ToolSpec::new("memory_dupes", "hygiene", false),
    ToolSpec::new("memory_reembed", "hygiene", false),
    ToolSpec::new("memory_reparse", "hygiene", false),
    ToolSpec::new("project_upsert", "core", true),
    ToolSpec::new("project_get", "core", true),
    ToolSpec::new("project_list", "core", true),
    ToolSpec::new("config_get", "config", false),
    ToolSpec::new("config_set", "config", false),
    ToolSpec::new("config_define", "config", false),
    ToolSpec::new("config_list", "config", false),
    ToolSpec::new("claim_acquire", "claim", false),
    ToolSpec::new("claim_release", "claim", false),
    ToolSpec::new("claim_list", "claim", false),
    ToolSpec::new("mail_send", "core", true),
    ToolSpec::new("mail_inbox", "core", true),
    ToolSpec::new("mail_read", "core", true),
    ToolSpec::new("mail_ack", "core", true),
    ToolSpec::new("mail_prune", "mail", false),
    ToolSpec::new("channel_list", "core", true),
    ToolSpec::new("channel_send", "core", true),
    ToolSpec::new("channel_history", "core", true),
    ToolSpec::new("channel_fetch", "core", true),
    ToolSpec::new("channel_present", "channel", false),
    ToolSpec::new("channel_invite", "channel", false),
    ToolSpec::new("channel_breach", "channel", false),
    ToolSpec::new("channel_join", "channel", false),
    ToolSpec::new("channel_leave", "channel", false),
    ToolSpec::new("channel_wait", "channel", false),
    ToolSpec::new("channel_members", "channel", false),
    ToolSpec::new("channel_create", "channel", false),
    ToolSpec::new("channel_set_gate", "channel", false),
    ToolSpec::new("channel_events", "channel", false),
    ToolSpec::new("channel_set_retention", "channel", false),
    ToolSpec::new("channel_prune", "channel", false),
    ToolSpec::new("mail_surrogate_grant", "mail", false),
    ToolSpec::new("mail_surrogate_revoke", "mail", false),
    ToolSpec::new("mail_surrogate_list", "mail", false),
];

pub fn spec(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|spec| spec.name == name)
}

pub fn is_core(name: &str) -> bool {
    spec(name).is_some_and(|spec| spec.core)
}

pub fn core_tools() -> impl Iterator<Item = &'static str> {
    TOOLS.iter().filter(|spec| spec.core).map(|spec| spec.name)
}

pub fn core_count() -> usize {
    TOOLS.iter().filter(|spec| spec.core).count()
}

pub(crate) fn group_of(name: &str) -> &'static str {
    spec(name)
        .expect("every route and every advertised tool is a row of TOOLS")
        .group
}
