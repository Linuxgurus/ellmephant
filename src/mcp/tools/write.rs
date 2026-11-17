//! The `write` tool group, split out of `mcp/mod.rs` (soma/ellmephant#50).
//!
//! Each group is its own `#[tool_router]` impl, which is what makes the split possible:
//! rmcp builds a `ToolRouter` from ONE impl block, so `mod.rs` composes the group routers
//! into the one table the handler dispatches through.

use crate::mcp::context::{caller_of, pen_name};
use crate::mcp::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};

#[tool_router(router = write_tool_router, vis = "pub(crate)")]
impl MemoryServer {
    /// Store one atomic memory; upserts by key. See the description for the write law.
    #[tool(
        name = "memory_upsert",
        description = UPSERT_DESCRIPTION,
        annotations(
            title = "Upsert memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_upsert(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<UpsertParams>,
    ) -> std::result::Result<String, String> {
        let claimed = p.as_pen.clone();
        let patch = p.into_patch();
        // The name travels on the CONNECTION, not the payload: a forged claim refuses the write
        // rather than silently becoming `anonymous`, because a lie with a signature is worse than
        // no signature at all. `as` may only ever name a DESCENDANT of that proven pen.
        let caller = caller_of(&ctx, self.declared.as_deref(), &self.paths)
            .pen()
            .map_err(|r| r.to_string())?
            .clone();
        let credit = match claimed {
            Some(name) => Some(crate::names::Pen::new(&name).map_err(|e| e.to_string())?),
            None => None,
        };
        // ★ THE PROVEN PEN, KEPT SEPARATE FROM THE CREDITED ONE (soma/ellmephant#192): authority is a
        // conjunction — the pen that sent this must be permitted AND the pen taking the credit must be
        // permitted, or `as:` becomes a way to spend one authority under another pen's name. The
        // stamping and the two gates live in `ops::memory_upsert`, so the HTTP door is the same law.
        let outcome = self
            .with_store("memory_upsert", move |s| {
                crate::ops::memory_upsert(s, &caller, crate::ops::Upsert { patch, credit })
            })
            .await?;
        tracing::info!(
            tool = "memory_upsert",
            key = %outcome.key,
            created = outcome.created,
            queued = outcome.queued,
            conflict = ?outcome.conflict,
            "memory stored"
        );
        // ★ A CONFLICT IS A RESULT, NOT AN ERROR: the caller asked "write if unchanged", the answer is
        // "it changed", and the current version is what it needs to retry. Reporting it as a tool
        // error would make a retry look like a failure of the store.
        if let Some(current) = outcome.conflict {
            return Ok(json_ok(serde_json::json!({
                "status": "conflict",
                "key": outcome.key,
                "current_version": current,
            })));
        }
        Ok(json_ok(serde_json::json!({
            "status": if outcome.created { "created" } else { "updated" },
            "key": outcome.key,
            "queued": outcome.queued,
        })))
    }

    /// Delete one memory by key.
    #[tool(
        name = "memory_delete",
        description = "Kill one memory by `key` — gone from search and from `memory_get`, no undo. Nothing is LOST: the delete snapshots the state it removes first, so its VERSIONS remain and the NEWEST of them is the content it held (`memory_history KEY`). `memory_history_erase` is the act that truly destroys. Use `memory_merge` when another memory should absorb it: a bare delete leaves every `[[key]]` pointing at it dangling, and `memory_stats` will report the store as damaged. Unsure rather than wrong? Reword it instead.",
        annotations(
            title = "Delete memory",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_delete(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let pen = caller_of(&ctx, self.declared.as_deref(), &self.paths)
            .pen()
            .map_err(|r| r.to_string())?
            .clone(); // writes are signed; anonymous is read-only
        let key = p.key;
        let rm_key = key.clone();
        // The receipt says what the delete did NOT remove. History outlives the row by design, and a
        // delete that stays silent about that reads as "gone" when it means "not findable as current".
        if let Some(remaining) = self
            .with_store("memory_delete", move |s| {
                crate::ops::memory_delete(s, &pen, &rm_key)
            })
            .await?
        {
            tracing::info!(tool = "memory_delete", key = %key, versions = remaining, "memory removed");
            Ok(json_ok(serde_json::json!({
                "status": "removed",
                "key": key,
                // The newest of these IS the state the memory was deleted in, because the delete
                // snapshots what it removes: the count is a count of recoverable states.
                "versions_remaining": remaining,
            })))
        } else {
            tracing::warn!(tool = "memory_delete", key = %key, "no such memory");
            Err(format!("no such memory: {key}"))
        }
    }

    /// Words over HISTORY — the third axis (address, meaning, words).
    #[tool(
        name = "memory_history_search",
        description = "Find the EXACT WORDS in any PAST state of any document — superseded, rewritten or deleted. The axis that answers hashes, keys and negations, where embeddings are blind. \n\nEvery hit is a PAST state, never a current row: it names the key, which version, when it was written, and whether the memory still exists (`orphan` — this is then the only copy of that text). It is never fused into `memory_search`, because mixing deleted text into a live page is the thing this store keeps having to undo.",
        annotations(
            title = "Search memory history",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_history_search(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<HistorySearchParams>,
    ) -> std::result::Result<String, String> {
        let _pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let limit = p.k.unwrap_or(10).clamp(1, 200);
        let hits = self
            .with_store("memory_history_search", move |s| {
                s.history_search(&p.query, limit)
            })
            .await?;
        let rows: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "key": h.key,
                    "version": h.n,
                    "written_at": h.written_at,
                    "orphan": h.orphan,
                    "state": "past",
                    "snippet": h.snippet,
                })
            })
            .collect();
        Ok(json_ok(serde_json::json!({ "hits": rows })))
    }

    /// The store's only true destroyer: erase deleted keys' history.
    #[tool(
        name = "memory_history_erase",
        description = "Erase HISTORY — the versions a delete keeps. This is the store's only act that destroys text: `memory_delete` snapshots the row and keeps its versions, so every deleted key still holds its body in full, and a hygiene sweep reduces ROWS while the BYTES stay. \n\n`orphans: true` erases every key whose memory row is gone (the pile that grows by itself), in one transaction, and reports how many keys and versions went. `key: <slug>` erases one, and only if its row is already gone — a live row's history is the only copy of what it said, so delete first, then erase. \n\n`confirm: true` is required: nothing can restore this.",
        annotations(
            title = "Erase memory history",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_history_erase(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<HistoryEraseParams>,
    ) -> std::result::Result<String, String> {
        if p.confirm != Some(true) {
            return Err(
                "this destroys text and nothing can restore it: pass `confirm: true`".to_string(),
            );
        }
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        match (p.key, p.orphans.unwrap_or(false)) {
            (Some(key), false) => {
                let done = self
                    .with_store("memory_history_erase", move |s| {
                        s.erase_history(&key).map(|n| (key.clone(), n))
                    })
                    .await?;
                Ok(json_ok(serde_json::json!({
                    "status": "erased",
                    "key": done.0,
                    "versions_erased": done.1,
                })))
            }
            (None, true) => {
                let (keys, versions) = self
                    .with_store("memory_history_erase", move |s| s.erase_orphan_history())
                    .await?;
                tracing::info!(tool = "memory_history_erase", keys, versions, pen = %pen, "orphan history erased");
                Ok(json_ok(serde_json::json!({
                    "status": "erased",
                    "orphan_keys": keys,
                    "versions_erased": versions,
                })))
            }
            _ => Err(
                "pass exactly one of `key` (one deleted key) or `orphans: true` (all of them)"
                    .to_string(),
            ),
        }
    }

    /// Continue a subject with one line — the cheap verb (`memory_append`).
    #[tool(
        name = "memory_append",
        description = APPEND_DESCRIPTION,
        annotations(
            title = "Append a line",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_append(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<AppendParams>,
    ) -> std::result::Result<String, String> {
        let pen = caller_of(&ctx, self.declared.as_deref(), &self.paths)
            .pen()
            .map_err(|r| r.to_string())?
            .clone();
        let key = p.key;
        let line = p.line;
        let force = p.force.unwrap_or(false);
        let log_key = key.clone();
        let out = self
            .with_store("memory_append", move |s| {
                crate::ops::memory_append(s, &pen, &key, &line, force)
            })
            .await?;
        tracing::info!(
            tool = "memory_append",
            key = %log_key,
            appended = out.appended,
            version = ?out.version,
            "memory appended"
        );
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// Fold one memory's lines into another without deleting the source — the append-only,
    /// cross-pen maintenance verb.
    #[tool(
        name = "memory_fold",
        description = FOLD_DESCRIPTION,
        annotations(
            title = "Fold memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_fold(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<FoldParams>,
    ) -> std::result::Result<String, String> {
        let pen = caller_of(&ctx, self.declared.as_deref(), &self.paths)
            .pen()
            .map_err(|r| r.to_string())?
            .clone();
        let from = p.from;
        let into = p.into;
        let log_from = from.clone();
        let log_into = into.clone();
        let out = self
            .with_store("memory_fold", move |s| {
                crate::ops::memory_fold(s, &pen, &from, &into)
            })
            .await?;
        tracing::info!(
            tool = "memory_fold",
            from = %log_from,
            into = %log_into,
            lines = out.integrated.len(),
            changed = out.changed,
            "memory folded"
        );
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// Integrate one memory into another and delete it — the safe way to merge.
    #[tool(
        name = "memory_merge",
        description = MERGE_DESCRIPTION,
        annotations(
            title = "Merge memory",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn memory_merge(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MergeParams>,
    ) -> std::result::Result<String, String> {
        // A merge DESTROYS the source, so it is signed like any other write.
        let pen = caller_of(&ctx, self.declared.as_deref(), &self.paths)
            .pen()
            .map_err(|r| r.to_string())?
            .clone();
        let from = p.from;
        let into = p.into;
        let log_from = from.clone();
        let log_into = into.clone();
        let out = self
            .with_store("memory_merge", move |s| {
                crate::ops::memory_merge(s, &pen, &from, &into)
            })
            .await?;
        tracing::info!(
            tool = "memory_merge",
            from = %log_from,
            into = %log_into,
            bodies = out.bodies,
            preloads = out.preloads,
            "memories merged"
        );
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// Take responsibility for a memory — responsibility, not authorship.
    #[tool(
        name = "memory_adopt",
        description = ADOPT_DESCRIPTION,
        annotations(
            title = "Adopt memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_adopt(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<AdoptParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let key = p.key;
        let force = p.force.unwrap_or(false);
        let owner = p.to.unwrap_or_else(|| pen.clone());
        let adopting = key.clone();
        let actor = pen.clone();
        let target = owner.clone();
        self.with_store("memory_adopt", move |s| {
            s.channel_gate(&actor, crate::store::channel::Class::Record)?;
            s.assign(&adopting, &actor, &target, force)
        })
        .await?;
        tracing::info!(tool = "memory_adopt", key = %key, actor = %pen, owner = %owner, force, "ownership assigned");
        Ok(json_ok(serde_json::json!({
            "status": "adopted",
            "key": key,
            "owner": owner,
        })))
    }
}
