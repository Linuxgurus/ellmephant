//! The `project` tool group, split out of `mcp/mod.rs` (soma/ellmephant#50).
//!
//! Each group is its own `#[tool_router]` impl, which is what makes the split possible:
//! rmcp builds a `ToolRouter` from ONE impl block, so `mod.rs` composes the group routers
//! into the one table the handler dispatches through.

use crate::mcp::*;
use crate::store::project;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};

#[tool_router(router = project_tool_router, vis = "pub(crate)")]
impl MemoryServer {
    /// Create or replace a project — the structures half of the store.
    #[tool(
        name = "project_upsert",
        description = PROJECT_UPSERT_DESCRIPTION,
        annotations(
            title = "Upsert project",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn project_upsert(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ProjectUpsertParams>,
    ) -> std::result::Result<String, String> {
        // writes are signed; anonymous is read-only
        let patch = p.into_patch();
        let outcome = self
            .write_outcome("project_upsert", &ctx, move |s, pen| {
                crate::ops::project_upsert(s, pen, &patch)
            })
            .await?;
        tracing::info!(
            tool = "project_upsert",
            key = %outcome.key,
            created = outcome.created,
            "project stored"
        );
        Ok(json_ok(serde_json::json!({
            "status": if outcome.created { "created" } else { "updated" },
            "key": outcome.key,
        })))
    }

    /// Read one project whole: its fields, its ordered preload, and the memories it owns.
    #[tool(
        name = "project_get",
        description = PROJECT_GET_DESCRIPTION,
        annotations(
            title = "Get project",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn project_get(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ProjectKeyParams>,
    ) -> std::result::Result<String, String> {
        let key = p.key;
        let hydrate = project::Hydrate::parse(p.hydrate.as_deref().unwrap_or("none"))
            .map_err(|e| e.to_string())?;
        let lookup = key.clone();
        let view = self
            .read_outcome("project_get", &ctx, move |s| {
                s.project_view(&lookup, hydrate)
            })
            .await?;
        tracing::debug!(
            tool = "project_get",
            key = %key,
            preload = view.preload.len(),
            "project read"
        );
        crate::wire::project::view_to_wire_json(&view).map_err(|e| e.to_string())
    }

    /// List the projects: the cheap index (no `state`), with intersecting filters.
    #[tool(
        name = "project_list",
        description = PROJECT_LIST_DESCRIPTION,
        annotations(
            title = "List projects",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn project_list(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ProjectListParams>,
    ) -> std::result::Result<String, String> {
        let filter = project::Filter {
            status: p.status,
            tags: p.tags,
            owner: p.owner,
            pattern: p.pattern,
            mode: p.mode.map(MatchModeParam::to_mode).unwrap_or_default(),
        };
        let rows = self
            .read_outcome("project_list", &ctx, move |s| {
                s.project_list(&filter)
            })
            .await?;
        tracing::info!(tool = "project_list", projects = rows.len(), "projects");
        crate::wire::project::list_to_wire_json(&rows).map_err(|e| e.to_string())
    }

    /// Read one config item — a stored value with a written ACL.
    #[tool(
        name = "config_get",
        description = "Read one CONFIG item: a small stored answer to `how should this run` (e.g. \
                       `recess_daily_time`), with TWO boundaries written on the row — `set_audience` \
                       (who may CHANGE the value) and `read_audience` (who may see it). Both use the \
                       memory audience grammar (`public`, `pen:<name>`, `subtree:<name>`, \
                       comma-separated), so ancestry and borrowed sessions work exactly as they do on \
                       a memory. Returns the item, or `null` when there is no such item OR it is not \
                       addressed to you: ABSENCE, not a refusal, because a refusal would confirm the \
                       item exists. Use `config_list` to see every item you may read.",
        annotations(
            title = "Read config",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn config_get(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ConfigGetParams>,
    ) -> std::result::Result<String, String> {
        let key = p.key;
        let item = self
            .read_outcome("config_get", &ctx, move |s| s.config_get(&key))
            .await?;
        tracing::info!(tool = "config_get", found = item.is_some(), "config read");
        Ok(json_ok(
            serde_json::to_value(&item).map_err(|e| e.to_string())?,
        ))
    }

    /// Change an existing config item's value.
    #[tool(
        name = "config_set",
        description = "Change an EXISTING config item's value. The item's `set_audience` is the gate: \
                       a caller it does not address is REFUSED and told the audience (a write refusal \
                       is reported — silence would look like success). This cannot create an item: a \
                       value with no written ACL would be whoever writes it first, and that is \
                       `config_define`'s job. The ACL is untouched here, so a pen that may write a \
                       value still cannot widen who else may. Returns the item as stored, with the \
                       new `owner` and `updated_at` — and that return is why WRITE IMPLIES READ: a \
                       writer can always learn the value it may not `config_get`, because the write \
                       hands the row back. `read_audience` bounds readers, not writers.",
        annotations(
            title = "Set config",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn config_set(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ConfigSetParams>,
    ) -> std::result::Result<String, String> {
        let ConfigSetParams { key, value, source } = p;
        let log_key = key.clone();
        let item = self
            .write_outcome("config_set", &ctx, move |s, pen| {
                s.config_set(pen.as_str(), &key, &value, source.as_deref())
            })
            .await?;
        tracing::info!(tool = "config_set", key = %log_key, "config written");
        Ok(json_ok(
            serde_json::to_value(&item).map_err(|e| e.to_string())?,
        ))
    }

    /// Create a config item or rewrite its ACL.
    #[tool(
        name = "config_define",
        description = "Create a config item, or rewrite one's ACL — the PROVISIONING act, separate \
                       from `config_set` because changing who may write a value is handing out \
                       authority, not editing it. Two rules stop a pen manufacturing reach: every \
                       address named in `set_audience` or `read_audience` must be one the caller can \
                       reach (itself, a pen below it, or anything under the root's break-glass), and \
                       an EXISTING item may only be redefined by a caller who could already WRITE it \
                       (its `set_audience`, not its `read_audience` — so a writer can republish a row \
                       it cannot read; that follows from write implying read). `subtree:` is the \
                       WIDEST grant this permits: one word hands the right to every descendant. \
                       `set_audience` may NOT be `public` — a value anyone can write is a shared \
                       scratchpad. `source` is REQUIRED: the reason belongs on the row. Returns the \
                       item as stored.",
        annotations(
            title = "Define config",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn config_define(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ConfigDefineParams>,
    ) -> std::result::Result<String, String> {
        let ConfigDefineParams {
            key,
            value,
            set_audience,
            read_audience,
            source,
        } = p;
        let log_key = key.clone();
        let item = self
            .write_outcome("config_define", &ctx, move |s, pen| {
                s.config_define(
                    pen.as_str(),
                    &key,
                    &value,
                    &set_audience,
                    &read_audience,
                    &source,
                )
            })
            .await?;
        tracing::info!(tool = "config_define", key = %log_key, "config defined");
        Ok(json_ok(
            serde_json::to_value(&item).map_err(|e| e.to_string())?,
        ))
    }

    /// List the config items this connection may read.
    #[tool(
        name = "config_list",
        description = "List the config items this connection may READ, in key order, optionally \
                       filtered by a name `prefix` (`recess` finds a namespace). Items outside your \
                       `read_audience` are simply absent — the same rule as a private memory, so a \
                       list can never disclose that an item exists. Use it before inventing a name: \
                       `config_get` on a name nobody defined returns `null`, which is not the same as \
                       an item you are not allowed to see.",
        annotations(
            title = "List config",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn config_list(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ConfigListParams>,
    ) -> std::result::Result<String, String> {
        let prefix = p.prefix;
        let items = self
            .read_outcome("config_list", &ctx, move |s| {
                s.config_list(prefix.as_deref())
            })
            .await?;
        tracing::info!(tool = "config_list", items = items.len(), "config listed");
        Ok(json_ok(serde_json::json!({ "config": items })))
    }
}
