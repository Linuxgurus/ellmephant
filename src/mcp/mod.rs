//! MCP server (P7) on `rmcp`: Streamable HTTP (default, the networked transport) or stdio.
//!
//! Tools: `memory_upsert`, `memory_delete`, `memory_search`,
//! `memory_get`, `memory_backlinks`, `memory_upvote`, `memory_stats`,
//! `memory_dupes` — with read-only annotations on the read tools and destructive on `delete`.
//! The tool descriptions carry the SPEC §9 write policy; that is where memory quality is
//! actually tuned. `memory_get` counts a `visits` read and carries the upvote cue — both in its
//! description *and* in the returned payload (`_nudge`), where the model's attention sits after
//! consuming a body; `memory_upvote` bumps the `times_useful` feedback counter.
//!
//! The store is one SQLite file behind a single connection (SPEC §15 #10), so the
//! server holds the [`Session`] behind a mutex and serves requests serially. SQLite and
//! the encoder are synchronous and can hold the lock for a while, so each tool runs its
//! store work on a blocking thread via [`tokio::task::spawn_blocking`] rather than
//! occupying a tokio worker.
//!
//! Two transports, both standard MCP: [`Transport::Http`] (the default) mounts the tool
//! surface at `/mcp` on a listening port via `rmcp`'s `StreamableHttpService` + `axum`;
//! [`Transport::Stdio`] keeps the client-launched pipe.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, ServiceExt, schemars, tool_handler};
use serde::Deserialize;

use crate::config::{Config, Paths};
use crate::store::session::Session;
// NOTE: do *not* import `crate::error::Result` here — `#[tool_handler]` expands to code that
// uses the unqualified `Result` from `rmcp`, and our single-parameter alias would shadow it.
use crate::error::Error;
use crate::model;
use crate::search::{Bundle, Field, MatchMode};
use crate::store::{self, project};

/// ONE TOOL TABLE (soma/ellmephant#231, #237).
///
/// The names used to live in four places — `TOOLS`, `CORE_TOOLS`, `Surface` and `group_of` — kept in
/// agreement by a test whose whole job was to catch them disagreeing. That test was the smell: four
/// lists of one fact. This is the fact. Everything else is a view of it.
///
/// `group` is a SHELF, not a permission: `GET /api` uses it to decide which short list a reader asks
/// for first, and every op is reachable regardless.
pub struct ToolSpec {
    /// The name in `tools/list`, and the name the REST route is derived from.
    pub name: &'static str,
    /// The API index shelf: `core`, `identity`, `config`, `claim`, `mail`, `channel`, `hygiene`.
    pub group: &'static str,
    /// Whether the hot-path door (`/mcp/core`) advertises it.
    pub core: bool,
}

pub const TOOLS: &[ToolSpec] = &[
    ToolSpec { name: "whoami", group: "core", core: true },
    ToolSpec { name: "memory_unowned", group: "hygiene", core: false },
    ToolSpec { name: "whois", group: "identity", core: false },
    ToolSpec { name: "pen_mint", group: "identity", core: false },
    ToolSpec { name: "pen_rotate", group: "identity", core: false },
    ToolSpec { name: "pen_lend", group: "identity", core: false },
    ToolSpec { name: "sudo", group: "identity", core: false },
    ToolSpec { name: "sudo_pass", group: "identity", core: false },
    ToolSpec { name: "sudo_release", group: "identity", core: false },
    ToolSpec { name: "sudo_sessions", group: "identity", core: false },
    ToolSpec { name: "memory_upsert", group: "core", core: true },
    ToolSpec { name: "memory_append", group: "hygiene", core: false },
    ToolSpec { name: "memory_delete", group: "core", core: true },
    ToolSpec { name: "memory_history_erase", group: "hygiene", core: false },
    ToolSpec { name: "memory_history_search", group: "hygiene", core: false },
    ToolSpec { name: "memory_merge", group: "hygiene", core: false },
    ToolSpec { name: "memory_fold", group: "hygiene", core: false },
    ToolSpec { name: "memory_adopt", group: "hygiene", core: false },
    ToolSpec { name: "memory_search", group: "core", core: true },
    ToolSpec { name: "semantic_search", group: "hygiene", core: false },
    ToolSpec { name: "memory_get", group: "core", core: true },
    ToolSpec { name: "memory_history", group: "core", core: true },
    ToolSpec { name: "memory_backlinks", group: "core", core: true },
    ToolSpec { name: "memory_dangling", group: "hygiene", core: false },
    ToolSpec { name: "memory_upvote", group: "core", core: true },
    ToolSpec { name: "memory_stats", group: "core", core: true },
    ToolSpec { name: "memory_health", group: "hygiene", core: false },
    ToolSpec { name: "memory_dupes", group: "hygiene", core: false },
    ToolSpec { name: "memory_reembed", group: "hygiene", core: false },
    ToolSpec { name: "memory_reparse", group: "hygiene", core: false },
    ToolSpec { name: "project_upsert", group: "core", core: true },
    ToolSpec { name: "project_get", group: "core", core: true },
    ToolSpec { name: "project_list", group: "core", core: true },
    ToolSpec { name: "config_get", group: "config", core: false },
    ToolSpec { name: "config_set", group: "config", core: false },
    ToolSpec { name: "config_define", group: "config", core: false },
    ToolSpec { name: "config_list", group: "config", core: false },
    ToolSpec { name: "claim_acquire", group: "claim", core: false },
    ToolSpec { name: "claim_release", group: "claim", core: false },
    ToolSpec { name: "claim_list", group: "claim", core: false },
    ToolSpec { name: "mail_send", group: "core", core: true },
    ToolSpec { name: "mail_inbox", group: "core", core: true },
    ToolSpec { name: "mail_read", group: "core", core: true },
    ToolSpec { name: "mail_ack", group: "core", core: true },
    ToolSpec { name: "mail_prune", group: "mail", core: false },
    ToolSpec { name: "channel_list", group: "core", core: true },
    ToolSpec { name: "channel_send", group: "core", core: true },
    ToolSpec { name: "channel_history", group: "core", core: true },
    ToolSpec { name: "channel_fetch", group: "core", core: true },
    ToolSpec { name: "channel_present", group: "channel", core: false },
    ToolSpec { name: "channel_invite", group: "channel", core: false },
    ToolSpec { name: "channel_breach", group: "channel", core: false },
    ToolSpec { name: "channel_join", group: "channel", core: false },
    ToolSpec { name: "channel_leave", group: "channel", core: false },
    ToolSpec { name: "channel_wait", group: "channel", core: false },
    ToolSpec { name: "channel_members", group: "channel", core: false },
    ToolSpec { name: "channel_create", group: "channel", core: false },
    ToolSpec { name: "channel_set_gate", group: "channel", core: false },
    ToolSpec { name: "channel_events", group: "channel", core: false },
    ToolSpec { name: "channel_set_retention", group: "channel", core: false },
    ToolSpec { name: "channel_prune", group: "channel", core: false },
    ToolSpec { name: "mail_surrogate_grant", group: "mail", core: false },
    ToolSpec { name: "mail_surrogate_revoke", group: "mail", core: false },
    ToolSpec { name: "mail_surrogate_list", group: "mail", core: false },
];

/// Which tools an MCP door ADVERTISES.
///
/// The store serves two doors over the same [`Session`] and the same auth: `/mcp` (the whole
/// surface) and `/mcp/core` (the hot path). The split exists because tool schemas are re-read on
/// every model step — the full surface is ~80 KB, and a client that can call the REST API does not
/// need most of it in context. The call path is NOT filtered: a core client that names a non-core
/// tool still reaches it, so this is an advertisement decision, never a permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// Every tool. `/mcp`.
    Full,
    /// The hot path: memory recall/write, projects, and communication. `/mcp/core`.
    Core,
}

/// The tools `/mcp/core` advertises — a VIEW of [`TOOLS`], never a second list. Everything else is
/// reachable over REST and indexed at `GET /api`.
///
/// Chosen by frequency, not by power: these are the calls a turn makes without thinking. Hygiene,
/// identity, admin and repair are rare and live on the expanded door and the API.
///
/// Four calls that look like siblings are deliberately outside it, and the reason is the
/// "without thinking" half rather than rarity: `semantic_search` is the wide door (every document
/// type, plus the graveyard) against `memory_search`'s cheap memory-only pass; `memory_append` and
/// `memory_fold` are edits whose correctness depends on a `memory_get` first, so the turn reads
/// before it writes; `channel_wait` blocks the turn for up to 25 seconds. Add one only with a
/// measurement of how often it is called, never because it reads like the others.

pub fn core_count() -> usize {
    TOOLS.iter().filter(|t| t.core).count()
}

/// Whether the hot-path door advertises `name`. Unknown names are not core: an unlisted tool is a
/// bug caught by the composition test, and guessing here would hide it.
pub fn is_core(name: &str) -> bool {
    TOOLS.iter().any(|t| t.core && t.name == name)
}

/// The row for one tool, or `None` when the name is not in the table.
pub fn spec(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|t| t.name == name)
}

/// The group an API index entry belongs to, read from the table. A name the table does not carry
/// falls back to `hygiene` — the same answer the old prefix match gave for `memory_*` — so a tool
/// added without its row still appears somewhere rather than vanishing from the index.
pub(crate) fn group_of(tool: &str) -> &'static str {
    spec(tool).map(|t| t.group).unwrap_or("hygiene")
}

/// The server's name, as `serverInfo` reports it — one source for both doors, so a boot and an
/// `initialize` cannot disagree about who is answering.
pub(crate) const SERVER_NAME: &str = "ellmephant";

/// One tool exactly as `tools/list` presents it, for a surface that is not MCP.
#[derive(Debug, Clone)]
pub(crate) struct ToolCard {
    /// Tool name.
    pub name: String,
    /// The law: what it does, when to use it, what it returns.
    pub description: String,
    /// The JSON Schema the model is handed as `inputSchema`.
    pub input_schema: serde_json::Value,
    /// The tool's annotation hints, as `tools/list` serializes them. The human-readable title lives
    /// in here (`annotations.title`), not at the top level — which is why a card has no `title`.
    pub annotations: serde_json::Value,
}

/// The server-level instruction — the same text an MCP `initialize` carries.
pub(crate) fn instructions() -> &'static str {
    INSTRUCTIONS
}

/// The tool surface as `tools/list` presents it.
///
/// Read from the router the `#[tool]` attributes build, so a door that is not MCP describes the
/// same toolkit from the same source instead of a hand-kept copy: a tool added below appears
/// here for free. `TOOLS` is the name contract; this is the content.
pub(crate) fn tool_catalog() -> Vec<ToolCard> {
    MemoryServer::tool_router()
        .list_all()
        .into_iter()
        .map(|tool| ToolCard {
            name: tool.name.to_string(),
            description: tool
                .description
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
            annotations: tool
                .annotations
                .as_ref()
                .and_then(|a| serde_json::to_value(a).ok())
                .unwrap_or(serde_json::Value::Null),
        })
        .collect()
}

/// Server-level instruction: the identity, the smart-caveman style, and the one duty that spans
/// every tool (curation), defined once here. Every other law lives in the tool descriptions — each
/// states its own, so nothing is written twice (SPEC §9).
const INSTRUCTIONS: &str = "ellmephant = Memory storage in smart-caveman style — big words in, \
small words out; drop filler, articles, glue. List-shaped reads return a positional table: a \
`cols` header names the fields, rows are bare arrays. Curation is part of the job, not an extra: \
when a memory shapes your task, `memory_upvote` it — the store's own rankings ride on your votes. \
A project is an activity over time — a container you can stand inside: \
`project_upsert` writes one, `project_get` reads one with its `preload`, `project_list` \
shows them all; a memory JOINS a project by carrying the `prj:<key>` TAG (search it — intersect \
with a topic tag to narrow to one facet) and POINTS at one \
from its body with `[[prj:key]]` (a bare `[[key]]` is a memory; keys are plain names, and the two \
tables may share one). \
Tool descriptions are the spec.";

impl MemoryServer {
    /// ★ ONE TOOL TABLE, BUILT FROM SEVEN ROUTERS (soma/ellmephant#50).
    ///
    /// `#[tool_router]` builds a `ToolRouter` from one impl block, so a 2,661-line impl was the
    /// only way to have 63 tools. The groups each export their own router now, and this composes
    /// them: `ToolRouter::merge` is rmcp's own API, so the table is still built by the library
    /// rather than by a hand-kept list that could drift from the `#[tool]` attributes.
    ///
    /// The order is irrelevant to behaviour (the map is keyed by name) and alphabetical by group
    /// so a diff cannot hide a new one.
    pub(crate) fn tool_router() -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        let mut router = Self::channel_tool_router();
        router.merge(Self::claim_tool_router());
        router.merge(Self::mail_tool_router());
        router.merge(Self::pen_tool_router());
        router.merge(Self::project_tool_router());
        router.merge(Self::read_tool_router());
        router.merge(Self::write_tool_router());
        router
    }
}

#[tool_handler(router = Self::tool_router())]
impl ServerHandler for MemoryServer {
    /// The surface split lives here and nowhere else. `#[tool_handler]` fills this method in only
    /// when the impl does not define one, so this override is the whole mechanism: one server, two
    /// advertised lists, one call path.
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let mut tools = Self::tool_router().list_all();
        if self.surface == Surface::Core {
            tools.retain(|tool| is_core(tool.name.as_ref()));
        }
        Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: None,
            cache_scope: None,
        })
    }

    fn get_info(&self) -> ServerConfig {
        // The core door says what it is NOT carrying, and where to find it. A hint in the
        // instructions costs one line here; a hidden tool with no signpost is an unfindable tool.
        let instructions = if self.surface == Surface::Core {
            format!(
                "{INSTRUCTIONS}\n\nCORE SURFACE: this door advertises the hot path only. The rest \
                 (hygiene, identity, admin, repair) is REST-only: GET /api for the map — each op's \
                 group, method, path and hints; `GET /api/<group>` for one shelf; `?full=true` for \
                 argument schemas. The expanded door is /mcp."
            )
        } else {
            INSTRUCTIONS.to_string()
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, crate::VERSION))
            .with_instructions(instructions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_is_a_subset_of_every_tool_and_none_is_duplicated() {
        let mut seen = std::collections::BTreeSet::new();
        for tool in TOOLS.iter().filter(|t| t.core) {
            assert!(seen.insert(tool.name), "core names a tool twice: {}", tool.name);
        }
        // The split has to be a SPLIT: a core list equal to the whole surface saves nothing.
        assert!(
            core_count() < TOOLS.len() / 2,
            "core is {} of {} — not a hot path",
            core_count(),
            TOOLS.len()
        );
        // The table is the only list: a duplicate name is a name with two answers.
        let mut names = std::collections::BTreeSet::new();
        for tool in TOOLS {
            assert!(names.insert(tool.name), "TOOLS names a tool twice: {}", tool.name);
        }
    }

    /// ★ THE COMPOSITION IS THE CONTRACT (soma/ellmephant#50). `MemoryServer::tool_router` merges
    /// seven group routers; a group forgotten in that merge would still compile, still pass the core
    /// tests, and quietly stop serving its tools. This is the test that makes the merge checkable.
    #[test]
    fn the_composed_router_carries_exactly_tools() {
        let mut served: Vec<String> = MemoryServer::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        served.sort();
        let mut expected: Vec<String> = TOOLS.iter().map(|t| t.name.to_string()).collect();
        expected.sort();
        assert_eq!(
            served, expected,
            "the composed router and `TOOLS` disagree — a group router is missing from the merge in \
             `MemoryServer::tool_router`, or a `#[tool]` was added without its name in `TOOLS`"
        );
    }

    #[test]
    fn the_core_surface_advertises_exactly_core() {
        let full = MemoryServer::tool_router().list_all();
        let mut core = full.clone();
        core.retain(|tool| is_core(tool.name.as_ref()));
        assert_eq!(core.len(), core_count());
        for tool in &full {
            let expected = is_core(tool.name.as_ref());
            assert_eq!(
                core.iter().any(|t| t.name == tool.name),
                expected,
                "{} is on the wrong side of the split",
                tool.name
            );
        }
    }

    #[test]
    fn every_tool_has_an_api_group() {
        let groups: std::collections::BTreeSet<&str> =
            TOOLS.iter().map(|tool| group_of(tool.name)).collect();
        assert_eq!(
            groups,
            [
                "channel", "claim", "config", "core", "hygiene", "identity", "mail"
            ]
            .into_iter()
            .collect(),
            "a new tool needs a group, and an empty group is a stale match arm"
        );
    }

    #[test]
    fn router_exposes_every_tool_with_annotations() {
        let tools = MemoryServer::tool_router().list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for expected in TOOLS {
            assert!(
                names.contains(&expected.name),
                "missing {:?}: {names:?}",
                expected.name
            );
        }

        let find = |name: &str| {
            tools
                .iter()
                .find(|t| t.name.as_ref() == name)
                .unwrap_or_else(|| panic!("tool {name:?} not found"))
        };
        let ann = |name: &str| find(name).annotations.clone().expect("annotations");

        // Read tools declare themselves read-only.
        for name in [
            "memory_search",
            "memory_get",
            "memory_backlinks",
            "memory_stats",
            "memory_health",
            "memory_dupes",
        ] {
            assert_eq!(
                ann(name).read_only_hint,
                Some(true),
                "{name} must be read-only"
            );
        }
        // Delete is the destructive one; upsert is an idempotent write.
        assert_eq!(ann("memory_delete").destructive_hint, Some(true));
        assert_eq!(ann("memory_delete").read_only_hint, Some(false));
        // Feedback writes but is neither read-only nor destructive.
        assert_eq!(ann("memory_upvote").read_only_hint, Some(false));
        assert_eq!(ann("memory_upvote").destructive_hint, Some(false));
        assert_eq!(ann("memory_upsert").idempotent_hint, Some(true));
        // Re-embed writes, but only to the derived index -- nothing it touches is source data.
        assert_eq!(ann("memory_reembed").read_only_hint, Some(false));
        assert_eq!(ann("memory_reembed").destructive_hint, Some(false));
        assert_eq!(ann("memory_reembed").idempotent_hint, Some(true));

        // The write *policy* rides on the upsert description (SPEC §9)...
        let upsert = find("memory_upsert")
            .description
            .clone()
            .expect("description");
        assert!(upsert.to_lowercase().contains("one topic"), "{upsert}");
        assert!(upsert.to_lowercase().contains("merge"), "{upsert}");
        // The pre-write split nudge: *evaluate* ("before saving"), then *action* (split, one call each).
        assert!(upsert.to_lowercase().contains("before saving"), "{upsert}");
        assert!(upsert.to_lowercase().contains("split"), "{upsert}");
        // Growth law is *coherence, not size*: deepen a topic, never add a second; no "never grow".
        assert!(
            upsert.to_lowercase().contains("deepen the topic"),
            "{upsert}"
        );
        assert!(!upsert.to_lowercase().contains("never grow"), "{upsert}");
        // The feedback nudge must carry a *trigger*, not just a quality bar.
        let upvote = find("memory_upvote")
            .description
            .clone()
            .expect("description");
        assert!(
            upvote.to_lowercase().contains("shaped this task"),
            "{upvote}"
        );
        assert!(upvote.to_lowercase().contains("visit"), "{upvote}");
        // The old "no flattery" clause was a *brake* that starved the counter; it must stay gone,
        // replaced by a default-on push to vote.
        assert!(
            !upvote.to_lowercase().contains("no flattery"),
            "the withholding brake is back: {upvote}"
        );
        assert!(upvote.to_lowercase().contains("under-voting"), "{upvote}");
        // The sweep takes *many* keys: the schema exposes `keys`, never a single `key`.
        let upvote_props = find("memory_upvote")
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .expect("upvote schema has properties");
        assert!(upvote_props.contains_key("keys"), "{upvote_props:?}");
        assert!(!upvote_props.contains_key("key"), "{upvote_props:?}");
        // ...and the firing cue must ride the *consuming* read, not the write tool —
        // the model reads `memory_get`, uses the body, and there learns to upvote.
        let get = find("memory_get").description.clone().expect("description");
        assert!(get.contains("memory_upvote"), "{get}");
        // ...and the cue must also ride `memory_get`'s *result* payload, where the model's
        // attention actually sits after consuming the body (pinned end-to-end in tests/mcp.rs).
        assert!(GET_NUDGE.contains("memory_upvote"), "{GET_NUDGE}");
        // Curation is a cross-tool duty, so it is announced once in the server instructions too.
        assert!(
            INSTRUCTIONS.contains("memory_upvote") && INSTRUCTIONS.contains("Curation"),
            "{INSTRUCTIONS}"
        );
        // ...while field definitions live on the field schemas, not the description.
        let upsert_props = find("memory_upsert")
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .expect("upsert schema has properties");
        let source_desc = upsert_props
            .get("source")
            .and_then(|v| v.get("description"))
            .and_then(|v| v.as_str())
            .expect("source description");
        assert!(
            source_desc.to_lowercase().contains("proof"),
            "{source_desc}"
        );
        assert!(
            !upsert.contains("`source` ="),
            "field defs belong on the fields: {upsert}"
        );

        // The `start_date` field description must not promise the OLD behaviour. It has now been
        // wrong twice — once stamping at creation, once claiming it was never stamped — and the
        // description is read as law (SPEC §9), so a stale sentence there is a lie the model acts on.
        // Pin the two halves of the current rule: derive on leaving `todo`, and never clear.
        let start_desc = find("project_upsert")
            .input_schema
            .get("properties")
            .and_then(|v| v.get("start_date"))
            .and_then(|v| v.get("description"))
            .and_then(|v| v.as_str())
            .expect("start_date description")
            .to_lowercase();
        assert!(
            start_desc.contains("todo"),
            "the start_date description must state the derivation: {start_desc}"
        );
        assert!(
            start_desc.contains("never cleared"),
            "the start_date description must say it is never cleared: {start_desc}"
        );
        assert!(
            !start_desc.contains("stamped on first write"),
            "start_date is not stamped at creation time: {start_desc}"
        );

        // Search carries the semantic phrase (`query`), the lexical leg (`pattern` + `mode`), the
        // FIELD PREDICATES (`key`/`key_prefix`/`title`) and the structured filters. `key` was absent
        // until #114 on the reasoning that a known key is `memory_get`; the issue overruled it — the
        // only way to match a key was `pattern`, which scans key, title AND body, so "this row" and
        // "this word appears somewhere" were the same request.
        //
        // `status` is here ON PURPOSE and refuses: it filters projects, and a memory search that
        // silently ignored it would widen — the #87 lesson.
        let props = find("memory_search")
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .expect("search schema has properties");
        for gone in ["tag_match", "field", "match"] {
            assert!(
                !props.contains_key(gone),
                "memory_search must not expose {gone}: {props:?}"
            );
        }
        let mut searchable: Vec<&str> = props.keys().map(String::as_str).collect();
        searchable.sort();
        assert_eq!(
            searchable,
            [
                "created_after",
                "created_before",
                "expand",
                "fields",
                "k",
                "key",
                "key_prefix",
                "max_chars",
                "max_distance",
                "min_chars",
                "mode",
                "pattern",
                "query",
                // ★ The honest empty list (#215): a caller must be able to ask "did my phrase reach
                // anything real" and get NO rather than a lexical refill. This list is the wire
                // surface, so a new knob appears here deliberately or not at all.
                "semantic_only",
                "status",
                "tags",
                "title",
                "updated_after",
                "updated_before"
            ]
        );
        // The `mode` values live in the schema (a `MatchModeParam` enum), never in the description.
        let mode_oneof = find("memory_search")
            .input_schema
            .get("$defs")
            .and_then(|d| d.get("MatchModeParam"))
            .and_then(|f| f.get("oneOf"))
            .and_then(|v| v.as_array())
            .expect("MatchModeParam enum in the schema");
        let mode_vals: Vec<&str> = mode_oneof
            .iter()
            .filter_map(|v| v.get("const"))
            .filter_map(|v| v.as_str())
            .collect();
        for want in ["literal", "regex"] {
            assert!(
                mode_vals.contains(&want),
                "missing mode {want}: {mode_vals:?}"
            );
        }
        // The projection selection rides on `fields` (a hit returns `key` + `via` by default).
        assert!(props.contains_key("fields"), "search must expose `fields`");
        // The `fields` *values* live in the schema (a `FieldParam` enum), never in the description —
        // that is how the model learns them; the prose stays policy-only.
        let field_oneof = find("memory_search")
            .input_schema
            .get("$defs")
            .and_then(|d| d.get("FieldParam"))
            .and_then(|f| f.get("oneOf"))
            .and_then(|v| v.as_array())
            .expect("FieldParam enum in the schema");
        let field_vals: Vec<&str> = field_oneof
            .iter()
            .filter_map(|v| v.get("const"))
            .filter_map(|v| v.as_str())
            .collect();
        for want in ["key", "via", "title", "tags", "chars", "distance", "score"] {
            assert!(
                field_vals.contains(&want),
                "missing field {want}: {field_vals:?}"
            );
        }

        // The read law rides on the search description, not the server instructions.
        let search = find("memory_search")
            .description
            .clone()
            .expect("description");
        assert!(search.to_lowercase().contains("widen"), "{search}");
        // The filter/projection *definitions* are the schema's job — the desc must not restate them.
        assert!(
            !search.contains("min_chars") && !search.contains("max_chars"),
            "search desc must not restate field defs: {search}"
        );

        // The server instruction carries the identity + the smart-caveman style (defined
        // once); it must not restate the tool law (that rides on the tools).
        let info = MemoryServer::new(memory_session()).get_info();
        let instructions = info.instructions.expect("instructions");
        assert!(
            instructions.to_lowercase().contains("tool descriptions"),
            "{instructions}"
        );
        assert!(
            instructions.to_lowercase().contains("smart-caveman"),
            "{instructions}"
        );
        assert!(
            !instructions.contains("one topic")
                && !instructions.contains("merge")
                && !instructions.contains("widen"),
            "instructions must not repeat tool law: {instructions}"
        );
    }

    /// A throwaway session for inspecting server info (no store work is done).
    fn memory_session() -> Session {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_root(dir.path());
        paths.ensure().expect("create store root");
        let cfg = Config::default();
        let spec = crate::model::Spec::from_config(&cfg, &paths);
        Session::open(paths, cfg, spec).expect("open store")
    }
}

#[cfg(test)]
mod miss_answer_tests {
    //! ★ HOSTILE REVIEW OF #194: the diagnostic is a decision about facts, so it is tested as one.
    //! Every case here is a different sentence a caller can receive, and the two that matter are the
    //! middle ones: an author whose audience DOES include them must not be told it excludes them, and a
    //! stranger must not be told anything.
    use super::{MemoryServer, OwnRowMiss};

    #[test]
    fn an_owner_hidden_by_its_own_audience_is_told_so() {
        match MemoryServer::own_row_miss("trap", "cline", "cline", "pen:james", false) {
            OwnRowMiss::ExcludedByItsAudience(msg) => {
                assert!(msg.contains("it is YOURS"), "{msg}");
                assert!(msg.contains("pen:james"), "and quotes the audience: {msg}");
                assert!(msg.contains("pen:cline"), "and names what to add: {msg}");
                assert!(
                    msg.contains("PROTECTED"),
                    "and names the OTHER gate on that write, so the advice is not a loop: {msg}"
                );
            }
            _ => panic!("expected the audience explanation"),
        }
    }

    #[test]
    fn an_owner_who_could_see_it_gets_a_divergence_not_an_audience_excuse() {
        match MemoryServer::own_row_miss("k", "cline", "cline", "pen:cline", true) {
            OwnRowMiss::Divergence(msg) => assert!(msg.contains("divergence"), "{msg}"),
            _ => panic!("the audience includes them; the cause must be the divergence"),
        }
    }

    #[test]
    fn someone_elses_row_is_never_explained_to_a_stranger() {
        assert!(matches!(
            MemoryServer::own_row_miss("k", "cline", "james", "pen:james", false),
            OwnRowMiss::NotYours
        ));
    }
}

mod context;
mod params;
mod tools;
mod transport;

pub use transport::*;
// Params are re-exported where a caller already named them (`mcp::SearchParams`).
pub use params::*;

/// One JSON object as the tool's text result.
pub(crate) fn json_ok(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// The one serialization a tool result is allowed, for a body that logs its own fields first.
///
/// A tool that cannot render its own outcome is a bug, not an empty object: `json_ok`'s `{}`
/// fallback would hand a model a success with no content.
pub(crate) fn json_out<T: serde::Serialize>(value: &T) -> std::result::Result<String, String> {
    serde_json::to_string(value).map_err(|e| e.to_string())
}
/// The `memory_upvote` sweep, implemented ONCE for both doors.
///
/// The MCP tool and `POST /memory/{key}/upvote` share this, so they cannot drift in either the error
/// they raise or the JSON they return. That is the whole point of a second door: not a second
/// implementation, a second *way in*.
pub(crate) async fn upvote_sweep(
    server: &MemoryServer,
    reader: crate::store::audience::Reader,
    keys: Vec<String>,
) -> std::result::Result<String, String> {
    if keys.is_empty() {
        return Err("memory_upvote needs at least one key".to_string());
    }
    // ★ A COUNTER IS STILL A READ. "Upvoted" for a row the caller cannot see would confirm it exists,
    // and leaving it out of `missing` would confirm it twice. So the sweep is narrowed to visible
    // keys first, and an invisible key is reported exactly as one that was never written.
    let probe = keys.clone();
    let visible: Vec<String> = server
        .with_read("memory_upvote", reader, move |s| {
            Ok(probe
                .into_iter()
                .filter(|k| s.show(k).is_ok())
                .collect::<Vec<_>>())
        })
        .await?;
    // Everything the probe could not see is reported as missing — which is also what a key that was
    // never written reads as, and that is the point.
    let hidden: Vec<String> = keys
        .iter()
        .filter(|k| !visible.iter().any(|v| v == *k))
        .cloned()
        .collect();
    let keys = visible;
    let sweep = keys.clone();
    let (upvoted, mut missing) = server
        .with_store("memory_upvote", move |s| s.record_useful_many(&sweep))
        .await
        .map_err(|e| e.to_string())?;
    for key in hidden {
        if !missing.contains(&key) {
            missing.push(key);
        }
    }
    // A sweep that hit nothing is a mistake worth surfacing, not a silent zero.
    if upvoted.is_empty() {
        return Err(Error::NotFound(keys.join(", ")).to_string());
    }
    tracing::info!(
        tool = "memory_upvote",
        upvoted = upvoted.len(),
        missing = missing.len(),
        "marked useful"
    );
    let upvoted: Vec<serde_json::Value> = upvoted
        .into_iter()
        .map(|(key, times_useful)| serde_json::json!({ "key": key, "times_useful": times_useful }))
        .collect();
    let mut result = serde_json::json!({ "status": "useful", "upvoted": upvoted });
    if !missing.is_empty() {
        result["missing"] = serde_json::json!(missing);
    }
    Ok(json_ok(result))
}

mod server;
pub use server::MemoryServer;
pub(crate) use server::{OwnRowMiss, log_search};
