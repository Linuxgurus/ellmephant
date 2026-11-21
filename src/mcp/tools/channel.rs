use crate::mcp::context::{caller_of, pen_name};
use crate::mcp::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};

#[tool_router(router = channel_tool_router, vis = "pub(crate)")]
impl MemoryServer {
    #[tool(
        name = "channel_list",
        description = "List the channels this store knows, with each one's policy: `members` (open = every live self unless it opted out; list = invite-only), `gate` (none / writes / all), and the clock the grace runs on. For a PROVEN pen it also returns `unread` per channel and the current `breach`, if any. ★ A count is not a receipt: listing moves nothing. `channel_fetch` is the only thing that counts as listening.",
        annotations(
            title = "List channels",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_list(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<String, String> {
        let pen = caller_of(&ctx, None, &self.paths)
            .proven()
            .map(ToString::to_string);
        let caller = pen.clone();
        let out = self
            .with_read(
                "channel_list",
                crate::store::audience::Reader::everything(),
                move |s| {
                    let channels = s.channels()?;
                    let unread = match caller.as_deref() {
                        Some(p) => s.channel_unread(p)?,
                        None => Vec::new(),
                    };
                    let breach = match caller.as_deref() {
                        Some(p) => s.channel_breach(p)?,
                        None => None,
                    };
                    Ok((channels, unread, breach))
                },
            )
            .await?;
        let (channels, unread, breach) = out;
        serde_json::to_string(&serde_json::json!({
            "channels": channels,
            "unread": unread,
            "breach": breach,
            "listener": pen,
        }))
        .map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_send",
        description = "Say ONE message in one channel. This is a channel, not a memory: the message is a log line with a fan-out and a receipt per member, and it becomes a memory only when a pen writes one deliberately. A write needs a PROVEN pen. Returns the `seq` and who was told.",
        annotations(
            title = "Send to a channel",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn channel_send(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelSendParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let ChannelSendParams {
            channel,
            body,
            reply_to,
        } = p;
        let (who, chan) = (pen.clone(), channel.clone());
        let msg = self
            .with_store("channel_send", move |s| {
                s.channel_send(&who, &chan, &body, reply_to)
            })
            .await?;
        tracing::info!(tool = "channel_send", pen = %pen, channel = %channel, seq = msg.seq, "channel message sent");
        serde_json::to_string(&msg).map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_history",
        description = "Read a channel's LOG: what was said, regardless of whether this pen has listened. ★ This does NOT touch the delivery ledger — only `channel_fetch` does — so reading history can never be mistaken for being up to date.",
        annotations(
            title = "Read channel history",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_history(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelHistoryParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let ChannelHistoryParams {
            channel,
            since,
            limit,
        } = p;
        let (chan, since, limit) = (channel.clone(), since.unwrap_or(0), limit.unwrap_or(50));
        let rows = self
            .with_read(
                "channel_history",
                crate::store::audience::Reader::everything(),
                move |s| s.channel_history(&chan, since, limit),
            )
            .await?;
        serde_json::to_string(&serde_json::json!({ "channel": channel, "messages": rows }))
            .map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_fetch",
        description = "LISTEN to the channels: return everything this pen was told and has not yet fetched, and mark it fetched. ★ THIS is what clears a gate — `available_at` (told), `presented_at` (offered at a turn boundary) and `fetched_at` (listened) are three different facts, and only the third means the message reached the pen. Fetch before changing the record, not to keep a lock open.",
        annotations(
            title = "Fetch channel messages",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn channel_fetch(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelFetchParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let ChannelFetchParams { channel, limit } = p;
        let (who, chan, limit) = (pen.clone(), channel.clone(), limit.unwrap_or(50));
        let rows = self
            .with_store("channel_fetch", move |s| {
                s.channel_fetch(&who, chan.as_deref(), limit)
            })
            .await?;
        tracing::info!(tool = "channel_fetch", pen = %pen, fetched = rows.len(), "channel messages fetched");
        serde_json::to_string(&serde_json::json!({
            "fetched": rows.len(),
            "messages": rows,
        }))
        .map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_present",
        description = "Mark that a TURN BOUNDARY offered this pen its inbox. Called by whatever owns the turn (the spine, the harness), never by the model — the normal path is present-then-fetch in the same turn, and a turn should never BEGIN gated. Each call while messages stay unfetched is one MISS; a channel decides how many misses it tolerates before it locks.",
        annotations(
            title = "Mark a delivery opportunity",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn channel_present(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelPresentParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let who = pen.clone();
        let boundary = p.boundary;
        let touched = self
            .with_store("channel_present", move |s| {
                s.channel_present(&who, boundary.as_deref())
            })
            .await?;
        serde_json::to_string(&serde_json::json!({ "pen": pen, "marked": touched }))
            .map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_breach",
        description = "What, if anything, is gating this pen right now: the channel, the message `seq`, how many turn boundaries offered it, and how long ago. ★ Reading is never gated. `channel_fetch` clears it; `lock_max_secs` releases an old lock and RECORDS the expiry, because a lock that aged out silently is worse than one that held.",
        annotations(
            title = "Read the gate",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_breach(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<String, String> {
        let pen = caller_of(&ctx, None, &self.paths)
            .proven()
            .map(ToString::to_string);
        if pen.is_none() {
            return Ok(serde_json::json!({ "breach": null, "proven": false }).to_string());
        }
        let who = pen.clone().unwrap_or_default();
        let found = self
            .with_read(
                "channel_breach",
                crate::store::audience::Reader::everything(),
                move |s| s.channel_breach(&who),
            )
            .await?;
        serde_json::to_string(&serde_json::json!({ "breach": found, "proven": true }))
            .map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_join",
        description = "Join a channel. Open channels are opt-out, so joining is only needed to undo a leave or to take up an invitation to a `list` channel. ★ Joining `ops_team` also joins `emergency` in the same transaction: a team that runs the cluster must be reachable when it falls over.",
        annotations(
            title = "Join a channel",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_join(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelMemberParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let channel = p.channel;
        let (who, chan) = (pen.clone(), channel.clone());
        self.with_store("channel_join", move |s| s.channel_join(&who, &chan))
            .await?;
        Ok(serde_json::json!({ "pen": pen, "channel": channel, "member": true }).to_string())
    }

    #[tool(
        name = "channel_invite",
        description = "Put ANOTHER pen in a channel. ★ `estate` or `james` only: a `list` channel is invite-only, so an invitation has an inviter and a pen cannot add itself. Inviting to `ops_team` also puts the pen in `emergency` — a team that runs the cluster must be reachable when it falls over.",
        annotations(
            title = "Invite a pen to a channel",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_invite(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelInviteParams>,
    ) -> std::result::Result<String, String> {
        let actor = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let (who, guest, chan) = (actor.clone(), p.pen.clone(), p.channel.clone());
        self.with_store("channel_invite", move |s| {
            s.channel_invite(&who, &guest, &chan)
        })
        .await?;
        Ok(serde_json::json!({
            "channel": p.channel, "pen": p.pen, "member": true, "invited_by": actor
        })
        .to_string())
    }

    #[tool(
        name = "channel_leave",
        description = "Leave a channel. ★ Leaving `emergency` while on the ops team is REFUSED, with the way out named: leave `ops_team` first. Anyone may otherwise be in `emergency` — being told the cluster is going down should not require a title.",
        annotations(
            title = "Leave a channel",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_leave(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelMemberParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let channel = p.channel;
        let (who, chan) = (pen.clone(), channel.clone());
        self.with_store("channel_leave", move |s| s.channel_leave(&who, &chan))
            .await?;
        Ok(serde_json::json!({ "pen": pen, "channel": channel, "member": false }).to_string())
    }

    #[tool(
        name = "channel_wait",
        description = "WAIT for channel messages to arrive, up to `timeout_secs` (max 25), returning everything this pen was TOLD after `after`. ★ This is a WAIT, not a LISTEN: it moves no receipt and cannot make a pen look up to date. A body that wants to be woken by chat blocks here; a body that wants to answer it then calls `channel_fetch`.",
        annotations(
            title = "Wait for channel messages",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_wait(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelWaitParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let after = match p.after {
            Some(after) => after,
            None => {
                let who = pen.clone();
                self.with_read(
                    "channel_wait",
                    crate::store::audience::Reader::everything(),
                    move |s| s.channel_head(&who),
                )
                .await?
            }
        };
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(p.timeout_secs.unwrap_or(0).min(25));
        loop {
            let who = pen.clone();
            let rows = self
                .with_read(
                    "channel_wait",
                    crate::store::audience::Reader::everything(),
                    move |s| s.channel_since(&who, after, 200),
                )
                .await?;
            if !rows.is_empty() || std::time::Instant::now() >= deadline {
                let truncated = rows.len() >= 200;
                return serde_json::to_string(&serde_json::json!({
                    "after": after,
                    "messages": rows,
                    "truncated": truncated,
                }))
                .map_err(|e| e.to_string());
            }
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        }
    }

    #[tool(
        name = "channel_members",
        description = "WHO is in a channel. Effective membership, not the override rows: an OPEN channel is every live self minus those who left on purpose; a LIST channel is its explicit members. Fingers and `estate` are never members. Answers \"am I on the team?\" without reading SQL, and lets a pen verify the rule that joining `ops_team` also joins `emergency`.",
        annotations(
            title = "List channel members",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_members(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelMembersParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let channel = p.channel;
        let key = channel.clone();
        let rows = self
            .with_read(
                "channel_members",
                crate::store::audience::Reader::everything(),
                move |s| s.channel_members(&key),
            )
            .await?;
        serde_json::to_string(&serde_json::json!({ "channel": channel, "members": rows }))
            .map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_create",
        description = "Create ONE channel. `open` channels are opt-out and may be made by any proven pen — conversation costs nobody anything. ★ A channel with a GATE (`writes` or `all`) can only be created by `estate` or `james`: a lock stops a pen changing the record, so creating one is a decision with a name on it. A key is created once; there is no rename.",
        annotations(
            title = "Create a channel",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn channel_create(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelCreateParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let (who, key, title) = (pen.clone(), p.channel.clone(), p.title.clone());
        let members = p.members.unwrap_or_else(|| "open".to_string());
        let gate = p.gate.unwrap_or_else(|| "none".to_string());
        let made = self
            .with_store("channel_create", move |s| {
                s.channel_create(&who, &key, &title, &members, &gate)
            })
            .await?;
        tracing::info!(tool = "channel_create", pen = %pen, channel = %made.key, gate = %made.gate, "channel created");
        serde_json::to_string(&made).map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_set_gate",
        description = "Change a channel's gate (`none` / `writes` / `all`). ★ `estate` or `james` only, and EVERY change is written as a `channel_event` row — who, when, from what, to what. A lock that cannot be attributed is a lock nobody answers for.",
        annotations(
            title = "Set a channel's gate",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_set_gate(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelGateParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let (who, chan, gate) = (pen.clone(), p.channel.clone(), p.gate.clone());
        let row = self
            .with_store("channel_set_gate", move |s| {
                s.channel_set_gate(&who, &chan, &gate)
            })
            .await?;
        tracing::warn!(tool = "channel_set_gate", pen = %pen, channel = %row.key, gate = %row.gate, "channel gate changed");
        serde_json::to_string(&row).map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_events",
        description = "The recorded changes to a channel: creation, and every gate change — who, when, from what, to what. ★ This is the answer to \"who made this dangerous?\", and it is a stored row rather than a convention.",
        annotations(
            title = "Channel change history",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_events(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelEventsParams>,
    ) -> std::result::Result<String, String> {
        pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let channel = p.channel;
        let key = channel.clone();
        let rows = self
            .with_read(
                "channel_events",
                crate::store::audience::Reader::everything(),
                move |s| s.channel_events(&key),
            )
            .await?;
        serde_json::to_string(&serde_json::json!({ "channel": channel, "events": rows }))
            .map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_set_retention",
        description = "Set how long a channel keeps its message TEXT. `0` = forever (the default, and silence must never delete anything). ★ `estate` or `james` only, and recorded: how long a channel remembers is a decision, not a preference. ★ A prune empties `body` and sets `pruned_at`; the receipt survives, so 'you were told seq N' stays answerable.",
        annotations(
            title = "Set channel retention",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_set_retention(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelRetentionParams>,
    ) -> std::result::Result<String, String> {
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let (who, chan, keep) = (pen.clone(), p.channel.clone(), p.keep_secs);
        let row = self
            .with_store("channel_set_retention", move |s| {
                s.channel_set_retention(&who, &chan, keep)
            })
            .await?;
        tracing::warn!(tool = "channel_set_retention", pen = %pen, channel = %row.key, keep_secs = row.keep_secs, "channel retention changed");
        serde_json::to_string(&row).map_err(|e| e.to_string())
    }

    #[tool(
        name = "channel_prune",
        description = "Expire the TEXT of messages past their channel's `keep_secs`. ★ This is NOT an erasure of what happened: the body is emptied, `pruned_at` is set, and the message row, its `seq` and every delivery row survive — so 'you were told seq N' is still true. A channel with `keep_secs = 0` is never touched.",
        annotations(
            title = "Prune channel text",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn channel_prune(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ChannelPruneParams>,
    ) -> std::result::Result<String, String> {
        let who = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let pen = crate::names::Pen::new(&who).map_err(|e| e.to_string())?;
        let only = p.channel;
        let rows = self
            .with_store("channel_prune", move |s| {
                crate::ops::channel_prune(s, &pen, only.as_deref())
            })
            .await?;
        let total: usize = rows.iter().map(|r| r.emptied).sum();
        tracing::info!(
            tool = "channel_prune",
            emptied = total,
            "channel text pruned"
        );
        serde_json::to_string(&serde_json::json!({ "pruned": rows, "emptied": total }))
            .map_err(|e| e.to_string())
    }
}
