use crate::mcp::context::{caller_of, pen_name, reader_of};
use crate::mcp::*;
use crate::store::project;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};

#[tool_router(router = project_tool_router, vis = "pub(crate)")]
impl MemoryServer {
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
        let pen = caller_of(&ctx, self.declared.as_deref(), &self.paths)
            .pen()
            .map_err(|r| r.to_string())?
            .clone();
        let patch = p.into_patch();
        let outcome = self
            .with_store("project_upsert", move |s| {
                crate::ops::project_upsert(s, &pen, &patch)
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
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let key = p.key;
        let hydrate = project::Hydrate::parse(p.hydrate.as_deref().unwrap_or("none"))
            .map_err(|e| e.to_string())?;
        let lookup = key.clone();
        let view = self
            .with_read("project_get", reader.clone(), move |s| {
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
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let filter = project::Filter {
            status: p.status,
            tags: p.tags,
            owner: p.owner,
            pattern: p.pattern,
            mode: p.mode.map(MatchModeParam::to_mode).unwrap_or_default(),
        };
        let rows = self
            .with_read("project_list", reader.clone(), move |s| {
                s.project_list(&filter)
            })
            .await?;
        tracing::info!(tool = "project_list", projects = rows.len(), "projects");
        crate::wire::project::list_to_wire_json(&rows).map_err(|e| e.to_string())
    }

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
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let key = p.key;
        let item = self
            .with_read("config_get", reader, move |s| s.config_get(&key))
            .await?;
        tracing::info!(tool = "config_get", found = item.is_some(), "config read");
        Ok(json_ok(
            serde_json::to_value(&item).map_err(|e| e.to_string())?,
        ))
    }

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
        let who = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let ConfigSetParams { key, value, source } = p;
        let log_key = key.clone();
        let item = self
            .with_store("config_set", move |s| {
                s.config_set(&who, &key, &value, source.as_deref())
            })
            .await?;
        tracing::info!(tool = "config_set", key = %log_key, "config written");
        Ok(json_ok(
            serde_json::to_value(&item).map_err(|e| e.to_string())?,
        ))
    }

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
        let who = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let ConfigDefineParams {
            key,
            value,
            set_audience,
            read_audience,
            source,
        } = p;
        let log_key = key.clone();
        let item = self
            .with_store("config_define", move |s| {
                s.config_define(&who, &key, &value, &set_audience, &read_audience, &source)
            })
            .await?;
        tracing::info!(tool = "config_define", key = %log_key, "config defined");
        Ok(json_ok(
            serde_json::to_value(&item).map_err(|e| e.to_string())?,
        ))
    }

    #[tool(
        name = "config_unset",
        description = "REMOVE a config item, so `omit the item instead of storing a blank` becomes \
                       something a caller can actually do. The gate is the item's CURRENT \
                       `set_audience` — exactly `config_set`'s gate, so you cannot unset what you \
                       could not set (and write implies read: the caller is told the item was \
                       removed even if its `read_audience` never addressed it). An item that is \
                       already absent is not an error; it is the state you asked for, and the \
                       status says `absent`. This is how a mis-typed key is cleaned up and how a \
                       grant is withdrawn by the pen holding its boundary. NO VERSIONS: the value \
                       and its `source` are gone, so the reason is worth having written down \
                       before this call. Prefer `config_set` when the policy is right and only the \
                       value is wrong.",
        annotations(
            title = "Unset config",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn config_unset(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ConfigUnsetParams>,
    ) -> std::result::Result<String, String> {
        let who = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let ConfigUnsetParams { key } = p;
        let log_key = key.clone();
        let removed = self
            .with_store("config_unset", move |s| s.config_unset(&who, &key))
            .await?;
        tracing::info!(
            tool = "config_unset",
            key = %log_key,
            removed = removed.is_some(),
            "config unset"
        );
        Ok(json_ok(serde_json::json!({
            "status": if removed.is_some() { "removed" } else { "absent" },
            "key": log_key,
        })))
    }

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
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let prefix = p.prefix;
        let items = self
            .with_read("config_list", reader, move |s| {
                s.config_list(prefix.as_deref())
            })
            .await?;
        tracing::info!(tool = "config_list", items = items.len(), "config listed");
        Ok(json_ok(serde_json::json!({ "config": items })))
    }
}
