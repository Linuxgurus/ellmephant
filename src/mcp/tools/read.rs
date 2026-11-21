use crate::mcp::context::{pen_name, reader_of};
use crate::mcp::*;
use crate::search::{Fields, Query};
use crate::store::{self};
use crate::wire::links::backlinks_to_wire_json;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};

#[tool_router(router = read_tool_router, vis = "pub(crate)")]
impl MemoryServer {
    #[tool(
        name = "memory_unowned",
        description = "List the rows nobody owns, newest first, with WHY each is ownerless: `disclaimed` (somebody released it — the author is still named, and this is a supported, deliberate state: retiring a descendant can disclaim its rows) or `unrecorded` (no currently existing pen is named as author at all — either a write made with no pen, or a pen that is gone). These are the rows an owner can CLAIM: anyone may adopt an ownerless row. Use it after `memory_stats` says how many; a count without a list gets quoted and never acted on.",
        annotations(
            title = "Rows nobody owns",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_unowned(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<UnownedParams>,
    ) -> std::result::Result<String, String> {
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let limit = p.limit.unwrap_or(50).clamp(1, 500);
        let rows = self
            .with_read("memory_unowned", reader, move |s| s.unowned(limit))
            .await?;
        Ok(json_ok(
            serde_json::to_value(&rows).map_err(|e| e.to_string())?,
        ))
    }

    #[tool(
        name = "semantic_search",
        description = SEMANTIC_SEARCH_DESCRIPTION,
        annotations(
            title = "Semantic search (all types)",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn semantic_search(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<SemanticSearchParams>,
    ) -> std::result::Result<String, String> {
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        self.run_semantic(p, reader).await
    }

    pub(crate) async fn run_semantic(
        &self,
        p: SemanticSearchParams,
        reader: crate::store::audience::Reader,
    ) -> std::result::Result<String, String> {
        let query = p.query.unwrap_or_default();
        if query.trim().is_empty() {
            return Err("semantic_search needs a `query`".to_string());
        }
        let k = p.k.unwrap_or(5).max(1);
        let log_query = query.clone();
        if p.deleted.unwrap_or(false) {
            let md = p.max_distance;
            let hits = self
                .with_read("semantic_search", reader.clone(), move |s| {
                    s.search_deleted(&query, k, md)
                })
                .await?;
            let hits: Vec<serde_json::Value> = hits
                .into_iter()
                .map(|t| {
                    serde_json::json!({
                        "type": t.kind,
                        "key": t.key,
                        "distance": t.distance,
                        "deleted_at": t.deleted_at,
                    })
                })
                .collect();
            tracing::info!(
                tool = "semantic_search",
                deleted = true,
                hits = hits.len(),
                "graveyard searched"
            );
            return serde_json::to_string(&serde_json::json!({ "deleted": true, "hits": hits }))
                .map_err(|e| e.to_string());
        }
        if let Some(kind) = p.doc_type.as_deref() {
            let kind = kind.trim().to_string();
            if kind != "memory" {
                let probe = kind.clone();
                let n = self
                    .with_read("semantic_search", reader.clone(), move |s| {
                        s.vectors_of(&probe)
                    })
                    .await?;
                if n == 0 {
                    return Err(format!(
                        "no `{kind}` documents are indexed yet — this store can only search by \
meaning over the kinds that have vectors (memory does). Ask for one, or omit `type`."
                    ));
                }
            }
        }
        let _ = log_query;
        let params = SearchParams {
            query: Some(query),
            pattern: None,
            mode: None,
            k: Some(k),
            max_distance: p.max_distance,
            semantic_only: None,
            expand: None,
            tags: Vec::new(),
            min_chars: None,
            max_chars: None,
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
            key: None,
            key_prefix: None,
            title: None,
            status: None,
            fields: p.fields,
        };
        self.run_search(params, reader).await
    }

    #[tool(
        name = "memory_search",
        description = SEARCH_DESCRIPTION,
        annotations(
            title = "Search memory",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_search(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<SearchParams>,
    ) -> std::result::Result<String, String> {
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        self.run_search(p, reader).await
    }

    pub(crate) async fn run_search(
        &self,
        p: SearchParams,
        reader: crate::store::audience::Reader,
    ) -> std::result::Result<String, String> {
        let query = p.query;
        let pattern = p.pattern;
        let mode = p.mode.map(MatchModeParam::to_mode).unwrap_or_default();
        let k = p.k;
        let max_distance = p.max_distance;
        let expand = p.expand.unwrap_or(0);
        let tags = p.tags;
        let semantic_only = p.semantic_only;
        let min_chars = p.min_chars;
        let max_chars = p.max_chars;
        let created_after = p.created_after;
        let created_before = p.created_before;
        let updated_after = p.updated_after;
        let updated_before = p.updated_before;
        let key = p.key;
        let key_prefix = p.key_prefix.unwrap_or(false);
        let title = p.title;
        let status = p.status;
        let fields = Fields::selection(p.fields.into_iter().map(FieldParam::to_field));
        let log_query = query.clone();
        let started = std::time::Instant::now();
        let bundle = self
            .with_read("memory_search", reader.clone(), move |s| {
                let q = Query {
                    text: query,
                    pattern,
                    mode,
                    k: k.unwrap_or_else(|| s.default_k()),
                    max_distance: max_distance.or_else(|| s.default_max_distance()),
                    semantic_only: semantic_only.unwrap_or(false),
                    expand,
                    tags,
                    min_chars,
                    max_chars,
                    created_after,
                    created_before,
                    updated_after,
                    updated_before,
                    key,
                    key_prefix,
                    title,
                    status,
                };
                s.search(&q)
            })
            .await?;
        let json =
            crate::wire::search::to_wire_json(&bundle, &fields).map_err(|e| e.to_string())?;
        log_search(
            log_query.as_deref(),
            expand,
            &bundle,
            json.len(),
            started.elapsed().as_millis() as u64,
        );
        Ok(json)
    }

    #[tool(
        name = "memory_get",
        description = "Pull one memory whole by `key` — the only read that returns a body. If it shaped your task, `memory_upvote` it.",
        annotations(
            title = "Get memory",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_get(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<GetParams>,
    ) -> std::result::Result<String, String> {
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let key = p.key;
        if let Some(n) = p.version {
            let lookup = key.clone();
            let past = self
                .with_read("memory_get", reader.clone(), move |s| s.version(&lookup, n))
                .await?
                .ok_or_else(|| format!("no version {n} for {key}"))?;
            let mut v = serde_json::to_value(&past).map_err(|e| e.to_string())?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert("version".to_string(), serde_json::json!(n));
                obj.insert("_nudge".to_string(), serde_json::json!(GET_NUDGE));
            }
            return serde_json::to_string(&v).map_err(|e| e.to_string());
        }
        let lookup = key.clone();
        let (mut mem, relation) = match self
            .with_read("memory_get", reader.clone(), move |s| {
                let mem = s.show(&lookup)?;
                let relation = s.owner_relation(&mem);
                Ok((mem, relation))
            })
            .await
        {
            Ok(found) => found,
            Err(e) => {
                return Err(self.explain_miss(&key, e.to_string(), reader.clone()).await);
            }
        };
        if let Some(seen) = self.bump_visit(&key) {
            mem.visits = seen;
        }
        tracing::debug!(tool = "memory_get", key = %key, "memory fetched");
        let mut v = serde_json::to_value(&mem).map_err(|e| e.to_string())?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("_nudge".to_string(), serde_json::json!(GET_NUDGE));
            if let Some(walk) = relation {
                obj.insert("owner_relation".to_string(), serde_json::json!(walk));
            }
        }
        serde_json::to_string(&v).map_err(|e| e.to_string())
    }

    #[tool(
        name = "memory_history",
        description = HISTORY_DESCRIPTION,
        annotations(
            title = "Memory history",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_history(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<HistoryParams>,
    ) -> std::result::Result<String, String> {
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let cap = p.limit.unwrap_or(HISTORY_LIMIT as u32) as usize;
        match p.key {
            Some(key) => {
                let lookup = key.clone();
                let rows = self
                    .with_read("memory_history", reader.clone(), move |s| {
                        s.versions(&lookup)
                    })
                    .await?;
                let total = rows.len();
                tracing::debug!(tool = "memory_history", key = %key, total, "history listed");
                Ok(json_ok(serde_json::json!({
                    "key": key,
                    "count": total,
                    "versions": rows.into_iter().take(cap).collect::<Vec<_>>(),
                })))
            }
            None => {
                let orphans = p.orphans_only;
                let rows = self
                    .with_read("memory_history", reader.clone(), move |s| {
                        s.version_index(orphans)
                    })
                    .await?;
                let total = rows.len();
                Ok(json_ok(serde_json::json!({
                    "count": total,
                    "keys": rows.into_iter().take(cap).collect::<Vec<_>>(),
                })))
            }
        }
    }

    #[tool(
        name = "memory_upvote",
        description = UPVOTE_DESCRIPTION,
        annotations(
            title = "Upvote memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn memory_upvote(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<KeysParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        upvote_sweep(self, reader, p.keys.into_vec()).await
    }

    #[tool(
        name = "memory_backlinks",
        description = "List memories whose body point at `key` with `[[…]]`.",
        annotations(
            title = "Backlinks",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_backlinks(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let key = p.key;
        let lookup = key.clone();
        let links = self
            .with_read("memory_backlinks", reader.clone(), move |s| {
                s.backlinks(&lookup)
            })
            .await?;
        tracing::debug!(tool = "memory_backlinks", key = %key, count = links.len(), "backlinks");
        backlinks_to_wire_json(&links).map_err(|e| e.to_string())
    }

    #[tool(
        name = "memory_dangling",
        description = "List every BROKEN `[[…]]` edge as a row: the referrer (`from`), the target that resolves to nothing (`to`), and the table it was expected in (`to_type`). ★ `memory_stats.dangling` counts DISTINCT MISSING TARGETS, not edges — two bodies pointing at one missing key are `dangling: 1` and TWO rows here, so the count can never tell you which body to repair. Repair by deleting the token, renaming the target, or writing the missing memory; an empty list is the only proof the count is honestly 0.",
        annotations(
            title = "Dangling links",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_dangling(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<String, String> {
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let edges = self
            .with_read("memory_dangling", reader, |s| s.dangling())
            .await?;
        tracing::debug!(
            tool = "memory_dangling",
            edges = edges.len(),
            targets = crate::store::links::dangling_targets(&edges),
            "dangling edges"
        );
        Ok(crate::wire::links::dangling_to_wire_json(&edges))
    }

    #[tool(
        name = "memory_health",
        description = "One call, five numbers, each carrying its definition: `body_chars`, `log_share` (eight operational-log key prefixes), `dup_pairs` (what `memory_dupes` counts: class `duplicate` only, default cosine distance 0.05 — NOT computed in this call; it is `null` with a `suppressed` line naming `memory_dupes`, the exact finder that returns the pairs), `woven` (one link edge, any direction, with `woven_inbound`/`woven_outbound` beside it) and `cold30` (visible rows older than 30 days that were never read). ★ These numbers are VISIBLE-scoped: they are computed through the caller's own Reader and the response says `scope: \"visible\"`; no store total is reported (shape (a), decided 2026-11-14). `suppressed` names any number that could not be computed — `cold30` is `null`, never `0`, when no visible row is older than 30 days, because `0` would assert 'nothing is cold'; `dup_pairs` is `null`, never `0`, because the finder is budgeted out of this call. Also returns rows, log_chars, dup_pairs_min_distance, cold30_population, tag_distinct, tag_singletons, rows_untagged, edges, edges_per_row and dangling. Read-only, no args, no writes.",
        annotations(
            title = "Memory health (five numbers)",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_health(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<String, String> {
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let health = self
            .with_read("memory_health", reader, |s| s.health())
            .await?;
        tracing::info!(
            tool = "memory_health",
            rows = health.rows,
            log_share = ?health.log_share,
            dup_pairs = health.dup_pairs,
            woven = ?health.woven,
            cold30 = ?health.cold30,
            "health"
        );
        serde_json::to_string(&health).map_err(|e| e.to_string())
    }

    #[tool(
        name = "memory_stats",
        description = "Read-only digest of the whole store — counts, size, tag/link shape, top tags, usage (`visits`/`times_useful`), top useful, age, and the ENCODE QUEUE (`encode_pending` documents still owing a vector, `encode_failed` those the encoder gave up on, `encode_oldest_s` how long the oldest has waited). A non-empty `encode_pending` is normal — a write schedules its document and returns; a growing `encode_oldest_s` means the worker is not keeping up. ★ `dangling` is a count of DISTINCT MISSING TARGETS, not edges: `memory_dangling` is the finder that names each broken edge.",
        annotations(
            title = "Store stats",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_stats(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<StatsParams>,
    ) -> std::result::Result<String, String> {
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let with_tags = p.tags.unwrap_or(false);
        let stats = self
            .with_read("memory_stats", reader.clone(), move |s| s.stats(with_tags))
            .await?;
        tracing::info!(tool = "memory_stats", memories = stats.memories, "stats");
        serde_json::to_string(&stats).map_err(|e| e.to_string())
    }

    #[tool(
        name = "memory_dupes",
        description = DUPES_DESCRIPTION,
        annotations(
            title = "Find duplicates",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_dupes(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<DupesParams>,
    ) -> std::result::Result<String, String> {
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let max_distance = p.max_distance.unwrap_or(store::dupes::DEFAULT_MAX_DISTANCE);
        let limit = p.limit.unwrap_or(DUPES_LIMIT);
        if p.plan.unwrap_or(false) {
            let plan = self
                .with_read("memory_dupes", reader.clone(), move |s| {
                    s.dupes_plan(max_distance, limit)
                })
                .await?;
            tracing::info!(
                tool = "memory_dupes",
                plan = true,
                steps = plan.steps.len(),
                refused = plan.refused.len(),
                max_distance,
                "dupes plan"
            );
            return crate::wire::dupes::plan_to_wire_json(&plan).map_err(|e| e.to_string());
        }
        let pairs = self
            .with_read("memory_dupes", reader.clone(), move |s| {
                s.dupes(max_distance, limit)
            })
            .await?;
        tracing::info!(
            tool = "memory_dupes",
            pairs = pairs.len(),
            max_distance,
            "dupes"
        );
        crate::wire::dupes::to_wire_json(&pairs).map_err(|e| e.to_string())
    }

    #[tool(
        name = "memory_reembed",
        description = REEMBED_DESCRIPTION,
        annotations(
            title = "Re-embed store",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_reembed(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(_): Parameters<ListParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let report = self
            .with_store("memory_reembed", move |s| s.reembed())
            .await?;
        tracing::info!(
            tool = "memory_reembed",
            pen = %pen,
            queued = report.queued,
            dim = report.dim,
            rebuilt = report.rebuilt,
            model = %report.model,
            "queued a re-embed of the store"
        );
        Ok(json_ok(serde_json::json!({
            "status": "queued",
            "queued": report.queued,
            "dim": report.dim,
            "rebuilt": report.rebuilt,
            "model": report.model,
        })))
    }

    #[tool(
        name = "memory_reparse",
        description = "Rebuild the `[[link]]` graph from every memory body — NO model, NO queue, \
                       milliseconds. Edge derivation happens at write time, so a textual rewrite \
                       (rename, merge), a parser change, or an older binary can leave edges the \
                       bodies no longer imply. `memory_reembed` repairs the same thing but loads the \
                       model and queues the whole store; run this first, and check `memory_stats` \
                       for `dangling` afterwards. Returns `rows` visited and `edges` the graph now \
                       holds.",
        annotations(
            title = "Re-parse link graph",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_reparse(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(_): Parameters<ListParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let report = self
            .with_store("memory_reparse", move |s| s.reparse())
            .await?;
        tracing::info!(
            tool = "memory_reparse",
            pen = %pen,
            rows = report.rows,
            edges = report.edges,
            "re-parsed the link graph"
        );
        Ok(json_ok(serde_json::json!({
            "status": "reparsed",
            "rows": report.rows,
            "edges": report.edges,
        })))
    }
}
