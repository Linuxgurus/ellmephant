//! The `mail` tool group, split out of `mcp/mod.rs` (soma/ellmephant#50).
//!
//! Each group is its own `#[tool_router]` impl, which is what makes the split possible:
//! rmcp builds a `ToolRouter` from ONE impl block, so `mod.rs` composes the group routers
//! into the one table the handler dispatches through.

use crate::mcp::context::pen_name;
use crate::mcp::*;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};

#[tool_router(router = mail_tool_router, vis = "pub(crate)")]
impl MemoryServer {
    /// Write one letter into a self's box.
    #[tool(
        name = "mail_send",
        description = "Write ONE letter into another self's box. This is a MAILBOX, not a message bus: the row is the whole event — nothing is delivered, no door is rung, and the sender NEVER learns whether it was read. ★ A letter is a CLAIM, NOT AN ORDER: it may be ignored, and there is deliberately no priority field and no verb in the schema — if you want something done, ASK in prose and let the reader decide. Address a SELF (`stet`, `twine`); a finger answers mail, it never receives it. Bodies may be large but stay under 1 MiB — put a real artifact in object storage and send a link plus a checksum. Returns the letter's `seq`, which the reader uses as its cursor.",
        annotations(
            title = "Send mail",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn mail_send(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailSendParams>,
    ) -> std::result::Result<String, String> {
        let MailSendParams {
            sender,
            recipient,
            subject,
            body,
            encoding,
        } = p;
        // The PROVEN pen decides who is speaking; the argument decides whose NAME is on the letter.
        // They may differ only while the proven pen holds a live `send` grant from the named pen, and
        // the store refuses otherwise. It used to discard the argument and stamp the typist — a
        // silent override that produced a real misattribution (2026-10-25, seq 566).
        let log_recipient = recipient.clone();
        let (log_sender, out) = self
            .write_outcome("mail_send", &ctx, move |s, proven| {
                let sender = match sender.trim() {
                    "" => proven.as_str().to_string(),
                    given => given.to_string(),
                };
                let out = s.mail_send(
                    proven.as_str(),
                    &sender,
                    &recipient,
                    subject.as_deref().unwrap_or(""),
                    &body,
                    encoding.as_deref().unwrap_or("text"),
                )?;
                Ok((sender, out))
            })
            .await?;
        tracing::info!(
            tool = "mail_send",
            sender = %log_sender,
            recipient = %log_recipient,
            seq = out.seq,
            bytes = out.bytes,
            "letter written"
        );
        json_out(&out)
    }

    /// Lend your OWN name to another pen for mail.
    #[tool(
        name = "mail_surrogate_grant",
        description = "Lend your OWN name: `surrogate` may then send letters whose `sender` is you. A grant is a row — scoped, dated, revocable — and every letter it produces records `sent_by`, so delegated mail is visible rather than assumed.",
        annotations(
            title = "Lend your name for mail",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn mail_surrogate_grant(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailGrantParams>,
    ) -> std::result::Result<String, String> {
        let grantor = p.grantor;
        let surrogate = p.surrogate;
        let scopes = p.scopes.unwrap_or_else(|| "send".to_string());
        let expires_at = p.expires_at;
        let note = p.note;
        self.write_tool("mail_surrogate_grant", &ctx, move |s, pen| {
            // A pen may lend its OWN name only: the check is on the proven caller, never on an
            // argument the caller chose.
            if !pen.as_str().eq_ignore_ascii_case(grantor.trim()) {
                return Err(crate::error::Error::Invalid(format!(
                    "refused: `{}` may grant its OWN name only; `{grantor}` must grant itself",
                    pen.as_str()
                )));
            }
            s.mail_grant(
                &grantor,
                &surrogate,
                &scopes,
                expires_at.as_deref(),
                note.as_deref(),
            )
        })
        .await
    }

    /// Take a lent name back.
    #[tool(
        name = "mail_surrogate_revoke",
        description = "Take a lent name back. Effective on the next call — nothing is cached.",
        annotations(
            title = "Revoke a mail surrogate",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn mail_surrogate_revoke(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailRevokeParams>,
    ) -> std::result::Result<String, String> {
        let grantor = p.grantor;
        let surrogate = p.surrogate;
        let n = self
            .write_outcome("mail_surrogate_revoke", &ctx, move |s, pen| {
                if !pen.as_str().eq_ignore_ascii_case(grantor.trim()) {
                    return Err(crate::error::Error::Invalid(format!(
                        "refused: `{}` may revoke its OWN grants only",
                        pen.as_str()
                    )));
                }
                s.mail_revoke(&grantor, &surrogate)
            })
            .await?;
        Ok(json_ok(serde_json::json!({ "revoked": n })))
    }

    /// Whose names may this pen speak under?
    #[tool(
        name = "mail_surrogate_list",
        description = "Every grant a pen holds, live or not — the question a reader asks before trusting a letter.",
        annotations(
            title = "List mail surrogates",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn mail_surrogate_list(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailSurrogatesParams>,
    ) -> std::result::Result<String, String> {
        // ★ The default subject is the CALLER, so this body needs the name before the read: the
        // envelope hands a read no pen, only the reader the store scopes it by.
        let caller = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let subject = p.surrogate.unwrap_or(caller);
        self.read_tool("mail_surrogate_list", &ctx, move |s| {
            s.mail_surrogates_of(&subject)
        })
        .await
    }

    /// What is in a box after a cursor. Bodies omitted; `wait_seconds` long-polls.
    #[tool(
        name = "mail_inbox",
        description = "What is in `recipient`'s box AFTER your cursor — envelopes only (seq, at, sender, subject, encoding, bytes), oldest first, bodies omitted so you can decide whether a body is worth fetching (`mail_read`). ★ THE CURSOR IS STORED, ONE ROW PER PROVEN PEN AND BOX (migration 0017): omit `since` and the store uses yours, so there is nothing to remember and no memory to keep in sync. The reply carries `cursor` (where you are), `head` (the top of the box) and `unread` — one call answers \"how much mail have I not read\". ★ THE ADVANCE IS THE COMMIT and it is a separate call (`mail_ack`), so a reader that dies before advancing sees the letters again; advance only AFTER dealing with them. `wait_seconds` LONG-POLLS: the call blocks up to 25s and returns the moment a letter lands, which is how a hand waits for mail without polling. No receipt is ever sent to the sender.",
        annotations(
            title = "Read inbox",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn mail_inbox(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailInboxParams>,
    ) -> std::result::Result<String, String> {
        let MailInboxParams {
            recipient,
            since,
            wait_seconds,
            limit,
        } = p;
        // ★ THE BOX AND THE READER ARE CHECKED BEFORE THE LONG POLL: waiting 25 seconds to be told you
        // may not read it is a worse answer than being told now. The reader is the PROVEN pen, which
        // is also the cursor's key — never an argument.
        let reader = self.mail_box_for(&ctx, &recipient, "read").await?;
        let limit = limit.unwrap_or(50).clamp(1, 500);
        let wait = wait_seconds
            .unwrap_or(0)
            .min(crate::store::mail::MAX_WAIT_SECONDS);

        // ★ THE LONG POLL IS THE RECIPIENT'S OWN: the reader holds the connection open and nobody
        // pushes into its room. That keeps "a letter is a claim" true at the transport level too.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);
        loop {
            let who = recipient.clone();
            let me = reader.clone();
            let (rows, head, cursor) = self
                .read_outcome("mail_inbox", &ctx, move |s| {
                    // Three facts, and they were being conflated: the top of THIS PAGE, the top of
                    // the BOX, and where the READER is. `max_seq` was read as "newest letter that
                    // exists" while it meant "highest seq returned" — which cost two readers an
                    // hour of theorising about a 273-letter discrepancy that never existed
                    // (measured 2026-10-21).
                    let stored = s.mail_cursor(&me, &who)?;
                    let seq = since.unwrap_or_else(|| stored.as_ref().map(|c| c.seq).unwrap_or(0));
                    Ok((s.mail_inbox(&who, seq, limit)?, s.mail_head(&who)?, stored))
                })
                .await?;
            if !rows.is_empty() || std::time::Instant::now() >= deadline {
                let page_top = rows.iter().map(|r| r.seq).max();
                return serde_json::to_string(&serde_json::json!({
                    "reader": cursor.as_ref().map(|c| c.reader.clone()).unwrap_or(reader),
                    "cursor": cursor.as_ref().map(|c| c.seq).unwrap_or(0),
                    "head": head,
                    "unread": (head - cursor.as_ref().map(|c| c.seq).unwrap_or(0)).max(0),
                    "letters": rows,
                    "page_top": page_top,
                    "max_seq": page_top.unwrap_or_else(|| since.unwrap_or(0)),
                    "newest_available": head,
                    "waited": wait > 0 && page_top.is_none(),
                }))
                .map_err(|e| e.to_string());
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Advance the caller's own cursor — the commit of an inbox read.
    #[tool(
        name = "mail_ack",
        description = "Move YOUR mail cursor in `recipient`'s box to `seq` — the commit of `mail_inbox`. ★ The cursor belongs to the PROVEN pen, so this call can only ever move your own row: another pen's cursor is not addressable from here, and moving yours backwards is refused rather than silently accepted (the error names where you actually are). Idempotent at the current value (`moved: 0`). The reply carries `seq`, `moved` (how many letters this skipped), `head` and `unread`, so \"how much is left\" needs no second call.",
        annotations(
            title = "Acknowledge mail",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn mail_ack(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailAckParams>,
    ) -> std::result::Result<String, String> {
        let MailAckParams { recipient, seq } = p;
        let reader = self.mail_box_for(&ctx, &recipient, "ack").await?;
        let (log_reader, log_recipient) = (reader, recipient.clone());
        let out = self
            .write_outcome("mail_ack", &ctx, move |s, pen| {
                s.mail_advance(pen.as_str(), &recipient, seq)
            })
            .await?;
        tracing::info!(
            tool = "mail_ack",
            reader = %log_reader,
            recipient = %log_recipient,
            seq = out.seq,
            moved = out.moved,
            unread = out.unread,
            "cursor advanced"
        );
        json_out(&out)
    }

    /// Turn a bare "no such memory" into a claim with its evidence (soma/ellmephant#57).
    ///
    /// A miss says something about the WHOLE store, and on 2026-10-21 it said the wrong thing for 42
    /// minutes while nobody could test which of four hypotheses was true. Nothing here changes the
    /// answer — a missing row is still an error — but it names the store, its size, its schema and the
    /// key's history, so the next reader can tell an absent key from a wrong file, a stale reader or a
    /// delete. Refusals that are NOT misses pass through untouched.
    pub(crate) async fn explain_miss(
        &self,
        key: &str,
        err: String,
        reader: crate::store::audience::Reader,
    ) -> String {
        if !err.contains("no such memory") {
            return err;
        }
        let lookup = key.to_string();
        let facts = match self
            .with_read("memory_get", reader.clone(), move |s| s.miss_facts(&lookup))
            .await
        {
            Ok(f) => f,
            // The diagnostic itself failed — say so rather than replacing one silent answer with
            // another; the original error is still the truth the caller asked for.
            Err(e) => {
                tracing::warn!(tool = "memory_get", key = %key, diagnostic = %e, "memory_get miss, and the diagnostic failed too");
                return err;
            }
        };
        tracing::warn!(
            tool = "memory_get",
            key = %key,
            store = %facts.store,
            memories = facts.memories,
            schema = facts.schema,
            versions = facts.versions,
            newest = ?facts.newest,
            "memory_get miss"
        );
        // ★ THE ONE BIT THAT HALVES THE SEARCH (#57): does the process's own WRITER connection see the
        // row? Reader-blind-only means a WAL/snapshot/path divergence; both blind means the store
        // genuinely does not hold it. Timed, because a miss must not queue behind a long encode.
        let writer_sees = {
            let k = key.to_string();
            match tokio::time::timeout(
                std::time::Duration::from_secs(2),
                self.with_store("memory_get", move |s| s.exists(&k)),
            )
            .await
            {
                Ok(Ok(true)) => "writer-sees-it",
                Ok(Ok(false)) => "writer-agrees-absent",
                Ok(Err(_)) => "writer-check-failed",
                Err(_) => "writer-check-timed-out",
            }
        };
        tracing::warn!(tool = "memory_get", key = %key, writer_sees, "memory_get miss: writer check");
        // ★ A HIDDEN ROW IS NOT A BROKEN READ (soma/ellmephant#189, 2026-10-26). The writer check is
        // raw SQL on the table, so it says "exists" for every row the reader's AUDIENCE hides — which
        // is the normal state of affairs in this estate, not a fault. Resolving that disagreement in
        // favour of "the read is lying" and telling the caller to *report the store* is how I spent an
        // hour hunting a split store that did not exist. The row's author is the one caller who may be
        // told the difference: it knows the row exists, so the truth leaks nothing.
        if writer_sees == "writer-sees-it" {
            if let Some(caller) = reader.pen_name() {
                let lookup = key.to_string();
                if let Ok(Some((owner, audience))) = self
                    .with_read("memory_get", reader.clone(), move |s| s.self_facts(&lookup))
                    .await
                {
                    // ★ ASK THE READER, DO NOT ASSUME (adversarial review of #194). The first version
                    // said "its audience does not include you" whenever the writer saw the row — which
                    // would be a FALSE cause for the one case that is genuinely a divergence: the
                    // audience DOES include the caller and the reader still cannot see it. Claiming a
                    // cause without testing it is the defect this whole issue is about.
                    // ★ ONE PLACE DECIDES (hostile review of #194, round 1): the caller's own row was
                    // tested twice — once inside `own_row_miss`, once here — so a change to one could
                    // leave the other answering the old question. The decision returns a value; this
                    // site renders it.
                    match Self::own_row_miss(key, caller, &owner, &audience, reader.sees(&audience))
                    {
                        OwnRowMiss::ExcludedByItsAudience(msg) => return msg,
                        OwnRowMiss::Divergence(msg) => return msg,
                        OwnRowMiss::NotYours => {}
                    }
                }
            }
            // Not the caller's row: absence is the honest answer, and inventing a fault is not.
            return err;
        }
        // ★ THE COUNT IS THE READER'S, AND SAYS SO (soma/ellmephant#189 D3). It used to read "holds N
        // memories", i.e. a statement about the file, while it was computed with the reader's own
        // predicate — so two pens comparing notes at the same moment saw a store that disagreed with
        // itself by eight rows, which is precisely the "wrong store" hypothesis the sentence invites.
        if facts.versions > 0 {
            format!(
                "no such memory: {key} — but {} holds {} version(s) for this key (newest {}), which you can \
                 see, and it is not in the live table. So this is a DELETED row with history, or a reader \
                 looking at the wrong store; `memory_history` lists the versions and `memory_get` with \
                 `version=N` fetches one. Do not treat this as 'never existed'. (You can see {} memories, \
                 schema {}.) [{}]",
                facts.store,
                facts.versions,
                facts.newest.as_deref().unwrap_or("unknown"),
                facts.memories,
                facts.schema,
                writer_sees
            )
        } else {
            format!(
                "no such memory: {key} — you can see {} memories in {} (schema {}), and neither this reader \
                 nor the writer connection has any history for this key, so it has never existed THERE — \
                 either the key is new, or you are reading a different store. [{}]",
                facts.memories, facts.store, facts.schema, writer_sees
            )
        }
    }

    /// The message for a miss that is really the AUTHOR's own row, hidden from the author by its
    /// audience — or `None` when that is not what happened.
    ///
    /// ★ PURE ON PURPOSE (adversarial review of #194): the decision is a question about three facts, and
    /// a diagnostic that can only be tested by standing up a transport is a diagnostic that will not be
    /// tested. `reader_can_see` is passed in rather than assumed — claiming "the audience excludes you"
    /// without checking is the same class of wrong cause this issue exists to remove.
    pub(crate) fn own_row_miss(
        key: &str,
        caller: &str,
        owner: &str,
        audience: &str,
        reader_can_see: bool,
    ) -> OwnRowMiss {
        if !owner.eq_ignore_ascii_case(caller) {
            return OwnRowMiss::NotYours;
        }
        if reader_can_see {
            // The writer sees it, this reader's own predicate says it may see it, and the read still
            // returned nothing: the ONE case where "the read is lying" is true rather than slander.
            return OwnRowMiss::Divergence(format!(
                "no such memory: {key} — but this process's writer connection sees it and its audience \
                 ({audience}) DOES include you. That is a real divergence between the reading and \
                 writing connections: report the store and this sentence rather than re-writing the \
                 memory."
            ));
        }
        OwnRowMiss::ExcludedByItsAudience(format!(
            "no such memory: {key} — but it EXISTS, it is YOURS (owner: {owner}), and its audience \
             ({audience}) does not include you, so a read may not return it. This is not a store fault \
             and not a deletion. Write it with an audience that includes `pen:{caller}` (or `public`) \
             and it comes back: that write narrows what the row exposes and is always yours to make. \
             If the row is PROTECTED, take its lease first (`claim_acquire`) — the audience is not the \
             only gate on that write."
        ))
    }

    /// One letter, body included.
    #[tool(
        name = "mail_read",
        description = "Fetch ONE letter whole, body included, by `seq`. Read the envelope in `mail_inbox` first: `bytes` and `encoding` are there so you can decide whether the body is worth the context, and how to interpret it (`text`, `json`, `gzip+base64`). `none` means that seq is gone — pruned, or never written; letters expire, memories do not.",
        annotations(
            title = "Read letter",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn mail_read(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailReadParams>,
    ) -> std::result::Result<String, String> {
        let seq = p.seq;
        let found = self
            .read_outcome("mail_read", &ctx, move |s| s.mail_read(seq))
            .await?;
        // ★ A LETTER IS PRIVATE CORRESPONDENCE: the box it is addressed to decides who may read it,
        // and the check happens on the LETTER, not on an argument the caller chose.
        if let Some(letter) = &found {
            self.mail_box_for(&ctx, &letter.recipient, "read").await?;
        }
        match found {
            Some(letter) => serde_json::to_string(&letter).map_err(|e| e.to_string()),
            None => Ok(serde_json::json!({ "letter": null, "seq": seq }).to_string()),
        }
    }

    /// Drop letters nobody ever read.
    #[tool(
        name = "mail_prune",
        description = "Delete letters older than `keep_days`, returning how many went. ★ THIS IS WHAT MAKES A MAILBOX NOT A MEMORY STORE: a memory is kept because it is TRUE, a letter is kept because NOBODY HAS READ IT YET — so once nobody plausibly will, it should go. Memories have no such path. Run it from a bell; there is no automatic expiry.",
        annotations(
            title = "Prune mail",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn mail_prune(
        &self,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(p): Parameters<MailPruneParams>,
    ) -> std::result::Result<String, String> {
        let keep_days = p.keep_days;
        // The envelope refuses an anonymous write with the same sentence `pen_name` did: the pruner
        // is signed like any other write.
        let removed = self
            .write_outcome("mail_prune", &ctx, move |s, _pen| s.mail_prune(keep_days))
            .await?;
        tracing::info!(tool = "mail_prune", keep_days, removed, "letters pruned");
        Ok(serde_json::json!({ "removed": removed, "keep_days": keep_days }).to_string())
    }
}
