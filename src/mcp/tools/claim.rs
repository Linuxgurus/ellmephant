//! The `claim` tool group, split out of `mcp/mod.rs` (soma/ellmephant#50).
//!
//! Each group is its own `#[tool_router]` impl, which is what makes the split possible:
//! rmcp builds a `ToolRouter` from ONE impl block, so `mod.rs` composes the group routers
//! into the one table the handler dispatches through.

use crate::mcp::context::pen_name;
use crate::mcp::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};

#[tool_router(router = claim_tool_router, vis = "pub(crate)")]
impl MemoryServer {
    /// Take or renew an atomic lease over a name — the cross-machine claim.
    #[tool(
        name = "claim_acquire",
        description = "Take (or renew) an atomic LEASE over a `name`. This is coordination between TWO writers to one store — the store versions but does not transact, so a bank `debit` (or any read-modify-write) can be lost when two writers interleave. The store owns this primitive because acquiring a client-side lease is itself a read-modify-write, and clocks cannot order the tie-break (two hands skew hours). ONE statement decides and writes: it succeeds when the name is free or the lease has EXPIRED, and is REFUSED while a DIFFERENT owner holds a live lease. The SAME owner re-acquiring is a renewal, not a conflict — names and owners are case-insensitive (`Hand-A` renews `hand-a`), and a renewal keeps the FIRST `acquired_at`. Returns `won` and the holder NOW — on a loss, `claim` names who holds it and until when. Hold it only around the critical section (claim → act and debit → `claim_release`), and pass a `ttl_seconds` that outlives the work but not the buyer: a crashed holder EXPIRES out of the way on its own. Not security — both writers are the same self; coordination only.",
        annotations(
            title = "Acquire claim",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn claim_acquire(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ClaimAcquireParams>,
    ) -> std::result::Result<String, String> {
        let ClaimAcquireParams {
            name,
            owner,
            ttl_seconds,
        } = p;
        // ★ WHO MAY LEASE. With `ELLMEPHANT_LEASERS` set, a lease is a privilege: the hands and the
        // terminal, never a browser chat. Without it, this stays open exactly as it was.
        let proven = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        crate::identity::may_lease(Some(&proven))?;
        // A PROVEN identity IS the owner of its lease, so the audit row cannot be signed with
        // someone else's name; an unproven caller's assertion stands, as before.
        let _asserted_owner = owner; // the pen decides; the argument is history
        let owner = proven;
        let out = self
            .with_store("claim_acquire", move |s| {
                s.claim_acquire(&name, &owner, ttl_seconds)
            })
            .await?;
        tracing::info!(
            tool = "claim_acquire",
            name = %out.claim.name,
            owner = %out.claim.owner,
            won = out.won,
            "claim attempted"
        );
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// Release a lease — only your own.
    #[tool(
        name = "claim_release",
        description = "Release a lease as the PROVEN pen: your own, or a descendant's with a `reason`. The holder may always release so a waiting writer can take it now instead of waiting out the TTL. An ANCESTOR of the holder may end a lease it did not take — the case that matters when a process died holding its own gate and only its own name could clear it — but only with a non-empty `reason`, because a quiet release is indistinguishable from a leak; the reply then carries `released_by` and `reason`. Anyone else is refused and told who holds it: a lease is not a lock you can steal, and the `owner` argument grants nothing (authority is checked against the ROW's owner). A lease that lapses is harmless — this is the tidy path, not a duty.",
        annotations(
            title = "Release claim",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn claim_release(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<ClaimReleaseParams>,
    ) -> std::result::Result<String, String> {
        // ★ WHO MAY END A LEASE (soma/spine#43 fix 4): the PROVEN pen, against the ROW's owner. The
        // holder releases its own; an ancestor releases a descendant's WITH a reason. The `owner`
        // argument no longer grants anything — it never should have.
        let actor = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let ClaimReleaseParams {
            name,
            owner,
            reason,
        } = p;
        let log_name = name.clone();
        let log_actor = actor.clone();
        let out = self
            .with_store("claim_release", move |s| {
                s.claim_release(&name, &actor, reason.as_deref())
            })
            .await?;
        tracing::info!(
            tool = "claim_release",
            name = %log_name,
            by = %log_actor,
            owner = ?owner,
            released = out.released,
            released_by = ?out.released_by,
            reason = ?out.reason,
            "claim release attempted"
        );
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// List the claims — one by name, or every claim, expired rows included.
    #[tool(
        name = "claim_list",
        description = "Read the claims: one by `name`, or EVERY claim when `name` is omitted. Each row carries its `owner`, when it was `acquired_at`, when it `expires_at`, and a derived `live` flag. EXPIRED rows are kept, not swept — an unexplained stale claim is a bug report, and this view is the only place to read it. Use this before blaming a writer for not proceeding: the answer is usually that someone else holds the lease, and the row says who and until when.",
        annotations(
            title = "List claims",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn claim_list(
        &self,
        Parameters(p): Parameters<ClaimListParams>,
    ) -> std::result::Result<String, String> {
        let name = p.name;
        let lookup = name.clone();
        let rows = self
            .with_read(
                "claim_list",
                crate::store::audience::Reader::everything(),
                move |s| s.claims(lookup.as_deref()),
            )
            .await?;
        tracing::info!(tool = "claim_list", claims = rows.len(), "claims listed");
        let mut out = serde_json::json!({ "claims": rows });
        if p.include_revocations.unwrap_or(false) {
            let lookup = name.clone();
            let rev = self
                .with_read(
                    "claim_list",
                    crate::store::audience::Reader::everything(),
                    move |s| s.claim_revocations(lookup.as_deref(), 50),
                )
                .await?;
            out["revocations"] = serde_json::json!(rev);
        }
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }
}
