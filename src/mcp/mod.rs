use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, ServiceExt, schemars, tool_handler};
use serde::Deserialize;

use crate::config::{Config, Paths};
use crate::error::Error;
use crate::model;
use crate::search::{Bundle, Field, MatchMode};
use crate::store::session::Session;
use crate::store::{self, project};

pub const TOOLS: &[&str] = &[
    "whoami",
    "memory_unowned",
    "whois",
    "pen_mint",
    "pen_rotate",
    "pen_lend",
    "sudo",
    "sudo_pass",
    "sudo_release",
    "sudo_sessions",
    "memory_upsert",
    "memory_append",
    "memory_delete",
    "memory_history_erase",
    "memory_history_search",
    "memory_merge",
    "memory_fold",
    "memory_adopt",
    "memory_search",
    "semantic_search",
    "memory_get",
    "memory_history",
    "memory_backlinks",
    "memory_dangling",
    "memory_upvote",
    "memory_stats",
    "memory_health",
    "memory_dupes",
    "memory_reembed",
    "memory_reparse",
    "project_upsert",
    "project_get",
    "project_list",
    "config_get",
    "config_set",
    "config_define",
    "config_list",
    "claim_acquire",
    "claim_release",
    "claim_list",
    "mail_send",
    "mail_inbox",
    "mail_read",
    "mail_ack",
    "mail_prune",
    "channel_list",
    "channel_send",
    "channel_history",
    "channel_fetch",
    "channel_present",
    "channel_invite",
    "channel_breach",
    "channel_join",
    "channel_leave",
    "channel_wait",
    "channel_members",
    "channel_create",
    "channel_set_gate",
    "channel_events",
    "channel_set_retention",
    "channel_prune",
    "mail_surrogate_grant",
    "mail_surrogate_revoke",
    "mail_surrogate_list",
];

pub(crate) const SERVER_NAME: &str = "ellmephant";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Full,
    Core,
}

pub const CORE_TOOLS: &[&str] = &[
    "whoami",
    "memory_search",
    "memory_get",
    "memory_upsert",
    "memory_delete",
    "memory_stats",
    "memory_backlinks",
    "memory_history",
    "memory_upvote",
    "project_get",
    "project_upsert",
    "project_list",
    "mail_send",
    "mail_inbox",
    "mail_read",
    "mail_ack",
    "channel_list",
    "channel_send",
    "channel_history",
    "channel_fetch",
];

pub(crate) fn group_of(tool: &str) -> &'static str {
    if CORE_TOOLS.contains(&tool) {
        return "core";
    }
    if tool.starts_with("pen_") || tool == "whois" || tool.starts_with("sudo") {
        "identity"
    } else if tool.starts_with("config") {
        "config"
    } else if tool.starts_with("claim") {
        "claim"
    } else if tool.starts_with("mail") {
        "mail"
    } else if tool.starts_with("channel") {
        "channel"
    } else {
        "hygiene"
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCard {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub annotations: serde_json::Value,
}

pub(crate) fn instructions() -> &'static str {
    INSTRUCTIONS
}

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
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let mut tools = Self::tool_router().list_all();
        if self.surface == Surface::Core {
            tools.retain(|tool| CORE_TOOLS.contains(&tool.name.as_ref()));
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
        for tool in CORE_TOOLS {
            assert!(TOOLS.contains(tool), "core names an unknown tool: {tool}");
            assert!(seen.insert(*tool), "core names a tool twice: {tool}");
        }
        assert!(
            CORE_TOOLS.len() < TOOLS.len() / 2,
            "core is {} of {} — not a hot path",
            CORE_TOOLS.len(),
            TOOLS.len()
        );
    }

    #[test]
    fn the_composed_router_carries_exactly_tools() {
        let mut served: Vec<String> = MemoryServer::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        served.sort();
        let mut expected: Vec<String> = TOOLS.iter().map(|t| t.to_string()).collect();
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
        core.retain(|tool| CORE_TOOLS.contains(&tool.name.as_ref()));
        assert_eq!(core.len(), CORE_TOOLS.len());
        for tool in &full {
            let expected = CORE_TOOLS.contains(&tool.name.as_ref());
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
            TOOLS.iter().map(|tool| group_of(tool)).collect();
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
            assert!(names.contains(expected), "missing {expected:?}: {names:?}");
        }

        let find = |name: &str| {
            tools
                .iter()
                .find(|t| t.name.as_ref() == name)
                .unwrap_or_else(|| panic!("tool {name:?} not found"))
        };
        let ann = |name: &str| find(name).annotations.clone().expect("annotations");

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
        assert_eq!(ann("memory_delete").destructive_hint, Some(true));
        assert_eq!(ann("memory_delete").read_only_hint, Some(false));
        assert_eq!(ann("memory_upvote").read_only_hint, Some(false));
        assert_eq!(ann("memory_upvote").destructive_hint, Some(false));
        assert_eq!(ann("memory_upsert").idempotent_hint, Some(true));
        assert_eq!(ann("memory_reembed").read_only_hint, Some(false));
        assert_eq!(ann("memory_reembed").destructive_hint, Some(false));
        assert_eq!(ann("memory_reembed").idempotent_hint, Some(true));

        let upsert = find("memory_upsert")
            .description
            .clone()
            .expect("description");
        assert!(upsert.to_lowercase().contains("one topic"), "{upsert}");
        assert!(upsert.to_lowercase().contains("merge"), "{upsert}");
        assert!(upsert.to_lowercase().contains("before saving"), "{upsert}");
        assert!(upsert.to_lowercase().contains("split"), "{upsert}");
        assert!(
            upsert.to_lowercase().contains("deepen the topic"),
            "{upsert}"
        );
        assert!(!upsert.to_lowercase().contains("never grow"), "{upsert}");
        let upvote = find("memory_upvote")
            .description
            .clone()
            .expect("description");
        assert!(
            upvote.to_lowercase().contains("shaped this task"),
            "{upvote}"
        );
        assert!(upvote.to_lowercase().contains("visit"), "{upvote}");
        assert!(
            !upvote.to_lowercase().contains("no flattery"),
            "the withholding brake is back: {upvote}"
        );
        assert!(upvote.to_lowercase().contains("under-voting"), "{upvote}");
        let upvote_props = find("memory_upvote")
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .expect("upvote schema has properties");
        assert!(upvote_props.contains_key("keys"), "{upvote_props:?}");
        assert!(!upvote_props.contains_key("key"), "{upvote_props:?}");
        let get = find("memory_get").description.clone().expect("description");
        assert!(get.contains("memory_upvote"), "{get}");
        assert!(GET_NUDGE.contains("memory_upvote"), "{GET_NUDGE}");
        assert!(
            INSTRUCTIONS.contains("memory_upvote") && INSTRUCTIONS.contains("Curation"),
            "{INSTRUCTIONS}"
        );
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
                "semantic_only",
                "status",
                "tags",
                "title",
                "updated_after",
                "updated_before"
            ]
        );
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
        assert!(props.contains_key("fields"), "search must expose `fields`");
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

        let search = find("memory_search")
            .description
            .clone()
            .expect("description");
        assert!(search.to_lowercase().contains("widen"), "{search}");
        assert!(
            !search.contains("min_chars") && !search.contains("max_chars"),
            "search desc must not restate field defs: {search}"
        );

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

pub use params::*;
pub use transport::*;

pub(crate) fn json_ok(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}
pub(crate) async fn upvote_sweep(
    server: &MemoryServer,
    reader: crate::store::audience::Reader,
    keys: Vec<String>,
) -> std::result::Result<String, String> {
    if keys.is_empty() {
        return Err("memory_upvote needs at least one key".to_string());
    }
    let probe = keys.clone();
    let visible: Vec<String> = server
        .with_read("memory_upvote", reader, move |s| {
            Ok(probe
                .into_iter()
                .filter(|k| s.show(k).is_ok())
                .collect::<Vec<_>>())
        })
        .await?;
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
