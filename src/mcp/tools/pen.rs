use crate::mcp::context::{caller_of, pen_name, reader_of};
use crate::mcp::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};

#[tool_router(router = pen_tool_router, vis = "pub(crate)")]
impl MemoryServer {
    #[tool(
        name = "whoami",
        description = "Report the identity this connection writes as: `asserted_by` (the `sender`/author a write is stamped with) and `proven` (true when `X-Stet-From` was matched against `X-Stet-Auth`). `proven: false` with `asserted_by: anonymous` means the connection presents no identity, so every write will be REFUSED (reads need no identity). Use this instead of guessing, and never to test credentials — it does not echo them.",
        annotations(
            title = "Who am I",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn whoami(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<String, String> {
        let caller = caller_of(&ctx, None, &self.paths);
        let body = match caller.denied() {
            None => match caller.proven() {
                Some(name) => serde_json::json!({ "asserted_by": name.as_str(), "proven": true }),
                None => serde_json::json!({
                    "asserted_by": crate::identity::ANONYMOUS,
                    "proven": false
                }),
            },
            Some(crate::identity::Denied::Forged(claimed)) => serde_json::json!({
                "asserted_by": crate::identity::ANONYMOUS,
                "proven": false,
                "claimed": claimed,
                "problem": "the X-Stet-Auth proof is missing or wrong",
            }),
            Some(crate::identity::Denied::Unavailable(why)) => serde_json::json!({
                "asserted_by": crate::identity::ANONYMOUS,
                "proven": false,
                "problem": format!("the credential store is unavailable: {why}"),
            }),
        };
        Ok(json_ok(body))
    }

    #[tool(
        name = "whois",
        description = "Look up one pen and its relationship to the pen asking. Returns `name`, `kind` (`self`/`finger`), `parent`, `created`, `ceased` (the tombstone of a retired pen), `owns` (how many rows it answers for) and `relation` — a walk in single letters from YOU to it: `ME` for yourself, `P` parent, `PP` grandparent, `C` child, `S` sibling, `PS` uncle, `SC` nephew, `PSC` cousin, with an English word in quotes only where English is exact. `relation: null` means no common root. A name is an address, never a permission; this is what the address is attached to.",
        annotations(
            title = "Who is this pen",
            read_only_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn whois(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<WhoisParams>,
    ) -> std::result::Result<String, String> {
        let viewer = pen_name(&ctx, self.declared.as_deref(), &self.paths)
            .unwrap_or_else(|_| crate::identity::ANONYMOUS.to_string());
        let pen = p.pen;
        let lookup = pen.clone();
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let facts = self
            .with_read("whois", reader, move |s| s.whois(&viewer, &lookup))
            .await?;
        Ok(json_ok(
            serde_json::to_value(&facts).map_err(|e| e.to_string())?,
        ))
    }

    #[tool(
        name = "pen_mint",
        description = "Create a pen under the CALLER. The parent is the pen that calls, never an argument: you may create downward, never sideways and never upward, because parentage is the grant. A hand mints a finger; `estate`'s children come from provisioning, which is a different door. Returns the new pen's name, kind, parent, and its SECRET — shown once, because the store keeps only its hash. Hand the secret to the child; the minter can impersonate the child until it rotates with `pen_rotate`. `kind` is `self` or `finger` and is never inferred from the name. ★ A NAME IS SPENT: a retired pen keeps its row and its name, so minting over a dead name is refused with the retirement time — generate a unique name (e.g. `finger-9f3a1c`).",
        annotations(
            title = "Mint a pen",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn pen_mint(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MintParams>,
    ) -> std::result::Result<String, String> {
        let caller = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let name = p.name;
        let kind = p.kind;
        let minted = self
            .with_store("pen_mint", move |s| s.mint(&caller, &name, &kind))
            .await?;
        tracing::info!(tool = "pen_mint", pen = %minted.name, kind = %minted.kind, parent = %minted.parent, "pen minted");
        Ok(json_ok(
            serde_json::to_value(&minted).map_err(|e| e.to_string())?,
        ))
    }

    #[tool(
        name = "pen_rotate",
        description = "Replace a credential and return the NEW secret, once. Omitted `pen` rotates YOURS; naming a pen rotates a DESCENDANT. A pen may rotate itself or a descendant — never a peer and never an ancestor, because authority is granted downward and borrowed only by policy. The old secret stops working on the very next call: authentication reads the row and caches nothing. Use this after minting (the minter knows the child's secret until the child rotates) and after a leak. There is no read-back verb; a lost secret is recovered by rotating again.",
        annotations(
            title = "Rotate a credential",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn pen_rotate(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<RotateParams>,
    ) -> std::result::Result<String, String> {
        let caller = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let target = p.pen.unwrap_or_else(|| caller.clone());
        let for_store = target.clone();
        let secret = self
            .with_store("pen_rotate", move |s| {
                s.rotate_credential(&caller, &for_store)
            })
            .await?;
        tracing::info!(tool = "pen_rotate", pen = %target, "credential rotated");
        Ok(json_ok(
            serde_json::json!({ "pen": target, "secret": secret }),
        ))
    }

    #[tool(
        name = "pen_lend",
        description = "Lend your own authority to a DESCENDANT (a finger you minted) for `secs`. This is what permits `memory_upsert` with `as: <that pen>`: parentage alone is NOT enough, because a lend EXPIRES and can be REVOKED, and an inheritance can do neither. Downward lending needs no policy row — parentage already grants it; `sudo` is for borrowing somebody else's authority across or upward, and that does need one. Returns the session: its id, the two pens, `expires_at`, and `via` (the session it was passed from, when it was).",
        annotations(
            title = "Lend authority down",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn pen_lend(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<LendParams>,
    ) -> std::result::Result<String, String> {
        let caller = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let pen = p.pen;
        let secs = p.secs;
        let session = self
            .with_store("pen_lend", move |s| s.lend(&caller, &pen, secs))
            .await?;
        tracing::info!(tool = "pen_lend", weak = %session.weak, powerful = %session.powerful, "authority lent down");
        Ok(json_ok(
            serde_json::to_value(&session).map_err(|e| e.to_string())?,
        ))
    }

    #[tool(
        name = "sudo",
        description = "Borrow `powerful`'s authority for `secs`. Needs a `sudoers` policy naming you as the borrower and this pair — authority is GRANTED, never assumed — and the duration cannot exceed the policy's ceiling. This is how `estate` is used at all: it is a real pen with no identity behind it, so nobody can authenticate as it and its universal reach is only ever somebody's borrow. Returns the session: id, both pens, `expires_at`. ★ No cache: every check reads the row, so a release or an expiry takes effect on the very next call.",
        annotations(
            title = "Borrow authority",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn sudo(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<SudoParams>,
    ) -> std::result::Result<String, String> {
        let caller = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let powerful = p.powerful;
        let secs = p.secs;
        let session = self
            .with_store("sudo", move |s| s.sudo(&caller, &powerful, secs))
            .await?;
        tracing::info!(tool = "sudo", weak = %session.weak, powerful = %session.powerful, "authority borrowed");
        Ok(json_ok(
            serde_json::to_value(&session).map_err(|e| e.to_string())?,
        ))
    }

    #[tool(
        name = "sudo_pass",
        description = "Hand a live session of yours DOWN to a descendant of yours, for no longer than you have left. Downward only: passing sideways would arm a peer, and passing up would return authority to its source. The receiver's session dies with yours. Returns the new session with `via` naming the one it came from.",
        annotations(
            title = "Pass a session down",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn sudo_pass(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<SudoPassParams>,
    ) -> std::result::Result<String, String> {
        let caller = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let powerful = p.powerful;
        let pen = p.pen;
        let secs = p.secs;
        let session = self
            .with_store("sudo_pass", move |s| {
                s.sudo_pass(&caller, &powerful, &pen, secs)
            })
            .await?;
        tracing::info!(tool = "sudo_pass", weak = %session.weak, powerful = %session.powerful, "session passed down");
        Ok(json_ok(
            serde_json::to_value(&session).map_err(|e| e.to_string())?,
        ))
    }

    #[tool(
        name = "sudo_release",
        description = "End your session for `powerful`, and every session passed down from it. Revocation cascades by `via`, so a hand letting go also ends its fingers' sessions — a revocation that left copies running would not be one. Takes effect on the next call: there is no lookup cache to expire.",
        annotations(
            title = "Release authority",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sudo_release(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<SudoReleaseParams>,
    ) -> std::result::Result<String, String> {
        let caller = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let powerful = p.powerful;
        let target = p.pen;
        let killed = self
            .with_store("sudo_release", move |s| {
                s.sudo_release_of(&caller, target.as_deref(), &powerful)
            })
            .await?;
        tracing::info!(tool = "sudo_release", killed, "authority released");
        Ok(json_ok(
            serde_json::json!({ "status": "released", "sessions": killed }),
        ))
    }

    #[tool(
        name = "sudo_sessions",
        description = "List LIVE borrowed authority: every session with its borrower, the pen it is borrowing, when it expires, whether it came from a policy or a lend, and which session it was passed from. Omit `pen` to see all pens (the operator's question before an audit); name one to see only that pen's. A session that has EXPIRED is not listed — expiry is the mechanism, and this is how you see it without guessing. Read-only.",
        annotations(
            title = "List borrowed authority",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sudo_sessions(
        &self,
        _ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<SudoSessionsParams>,
    ) -> std::result::Result<String, String> {
        let pen = p.pen;
        let sessions = self
            .with_store("sudo_sessions", move |s| s.sudo_sessions(pen.as_deref()))
            .await?;
        Ok(json_ok(
            serde_json::to_value(&sessions).map_err(|e| e.to_string())?,
        ))
    }
}
