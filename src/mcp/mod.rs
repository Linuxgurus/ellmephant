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

/// Names of the tools this server exposes.
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

/// The server's name, as `serverInfo` reports it — one source for both doors, so a boot and an
/// `initialize` cannot disagree about who is answering.
pub(crate) const SERVER_NAME: &str = "ellmephant";

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

/// The tools `/mcp/core` advertises. Everything else is reachable over REST and indexed at `GET /api`.
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

/// The group an API index entry belongs to. The group is a shelf, not a permission: `GET /api`
/// carries every op, and the group only decides which short list a reader asks for first.
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
        // memory_dupes, merge, adopt, unowned, dangling, reembed, reparse, history_*.
        "hygiene"
    }
}

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

/// The MCP server. Holds the single store [`Session`] behind a mutex; `#[tool_handler]`
/// reaches the tool table through the generated `Self::tool_router()`, so there is no field.
///
/// (This comment was found attached to `ChannelSendParams` a hundred lines away — two lines about
/// the server had drifted onto a params struct in an earlier copy-edit. It is back where it belongs.)
#[derive(Clone)]
pub struct MemoryServer {
    session: Arc<Mutex<Session>>,
    watch: Arc<crate::http::LockWatch>,
    /// Read-side handholds, taken once at construction so that a READ never has to touch `session`:
    /// the writer holds that for the whole of a write, embedding included, and a reader queueing on
    /// it is precisely the stall this split removes.
    paths: crate::config::Paths,
    cfg: crate::config::Config,
    encoder: Arc<super::store::session::Encoder>,
    /// ★ A DECLARED PEN, for transports that carry no HTTP request (stdio, in-process). A server
    /// started BY a process is as trusted as that process, so it may say who it is once, at
    /// construction; every write it makes is stamped with that name instead of being refused as
    /// anonymous. HTTP transports ignore this — their headers decide.
    declared: Option<String>,
    /// Which tools this instance advertises. One process serves both doors; the value is set once
    /// at mount time and never changes.
    surface: Surface,
}

/// What a miss means when the row belongs to the caller (hostile review of #194, round 1). Three
/// outcomes, decided once and rendered elsewhere: the first version tested the same two conditions in
/// two places, which is how a diagnostic comes to disagree with itself.
pub(crate) enum OwnRowMiss {
    /// The row exists, the caller owns it, and its audience excludes the caller: recoverable, and the
    /// caller may be told because it already knows the row exists.
    ExcludedByItsAudience(String),
    /// Writer sees it, the reader's own predicate says it may see it, the read returned nothing: a real
    /// divergence between the two connections.
    Divergence(String),
    /// Not the caller's row: absence, never a diagnosis.
    NotYours,
}

impl MemoryServer {
    /// Build a server over an open session — the `ellmephant mcp` path.
    pub fn new(session: Session) -> Self {
        Self::from_shared(Arc::new(Mutex::new(session)))
    }

    /// Build a server over a session already behind a shared lock (tests).
    pub fn from_shared(session: Arc<Mutex<Session>>) -> Self {
        Self::with_watch(session, Arc::new(crate::http::LockWatch::new()))
    }

    /// Build a server sharing a writer witness with the plain-HTTP surface.
    ///
    /// The read-side handholds are read out of the session here, once, while nothing else holds it.
    pub fn with_watch(session: Arc<Mutex<Session>>, watch: Arc<crate::http::LockWatch>) -> Self {
        let (paths, cfg, encoder) = {
            let guard = session.lock().unwrap_or_else(|e| e.into_inner());
            (guard.paths().clone(), guard.cfg().clone(), guard.encoder())
        };
        Self {
            session,
            watch,
            paths,
            cfg,
            encoder,
            declared: None,
            surface: Surface::Full,
        }
    }

    /// Advertise a surface other than the full one. Called at mount time by [`http_router`]; the
    /// session, auth and call path are unchanged, so a core door is the same server with a shorter
    /// `tools/list`.
    pub fn with_surface(mut self, surface: Surface) -> Self {
        self.surface = surface;
        self
    }

    /// ★ ONLY THE RECIPIENT READS ITS MAIL. `mail_inbox` takes a recipient as an argument, so without
    /// this any caller could read any box. A proven identity reads its own box, or a box an explicit
    /// surrogate map hands it; anonymous is refused before the box is even considered.
    async fn mail_box_for(
        &self,
        ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
        requested: &str,
        scope: &'static str,
    ) -> std::result::Result<String, String> {
        let who = pen_name(ctx, self.declared.as_deref(), &self.paths)?;
        let (reader, box_name) = (who.clone(), requested.to_string());
        let allowed = self
            .with_read(
                "mail_box_for",
                reader_of(ctx, self.declared.as_deref(), &self.paths),
                move |s| {
                    if scope == "ack" {
                        s.mail_may_ack(&reader, &box_name)
                    } else {
                        s.mail_may_read(&reader, &box_name)
                    }
                },
            )
            .await?;
        if allowed {
            return Ok(who);
        }
        Err(format!(
            "refused: `{who}` may not {scope} `{requested}`'s mail — only the recipient, or a live              `{scope}` grant, may"
        ))
    }

    /// Declare the pen for a transport with no HTTP request (stdio / in-process). Writes are then
    /// stamped with this name; without it they are REFUSED, because anonymous is read-only.
    pub fn with_identity(mut self, name: impl Into<String>) -> Self {
        self.declared = Some(name.into());
        self
    }

    /// The writer witness this server marks while a tool holds the store lock.
    pub fn watch(&self) -> Arc<crate::http::LockWatch> {
        Arc::clone(&self.watch)
    }

    /// Run `f` against the store on a blocking thread, turning an [`Error`] into a
    /// tool-error string. SQLite and the encoder can hold the store lock for a while, so
    /// this must not occupy a tokio worker.
    ///
    /// `tool` labels the call in logs: a success is a `debug!` with its latency, a store
    /// error a `warn!`, and a panic/join failure an `error!`. That is the one place every
    /// tool failure surfaces, so `docker logs` never loses it to a client-side error string.
    pub(crate) async fn with_store<T, F>(
        &self,
        tool: &'static str,
        f: F,
    ) -> std::result::Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session) -> crate::error::Result<T> + Send + 'static,
    {
        let session = Arc::clone(&self.session);
        let watch = Arc::clone(&self.watch);
        let started = std::time::Instant::now();
        let outcome = tokio::task::spawn_blocking(move || {
            // The single connection is a mutex; a long re-embed or model load queues here.
            let wait = std::time::Instant::now();
            let mut guard = session
                .lock()
                .map_err(|_| "memory store lock poisoned".to_string())?;
            tracing::debug!(
                tool,
                wait_ms = wait.elapsed().as_millis() as u64,
                "store lock acquired"
            );
            // Witness the holder for exactly as long as the lock is held.
            let _held = watch.held(tool);
            f(&mut guard).map_err(|e| e.to_string())
        })
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(Ok(value)) => {
                tracing::debug!(tool, elapsed_ms, "tool ok");
                Ok(value)
            }
            Ok(Err(err)) => {
                tracing::warn!(tool, elapsed_ms, error = %err, "tool error");
                Err(err)
            }
            Err(join) => {
                let err = format!("memory store task failed: {join}");
                tracing::error!(tool, elapsed_ms, error = %err, "tool task failed");
                Err(err)
            }
        }
    }

    /// Run `f` against the store on a READ-ONLY session of its own: its own SQLite connection, the
    /// shared encoder, and **no store lock at all**.
    ///
    /// This is the difference between a read that waits and a read that answers. A write holds
    /// `session` for its whole duration — the embedding included, and the embedding is the slow part
    /// (~1.3-8.5 s per KB of body) — so a reader that queued on that lock waited out the write. In
    /// WAL mode a reader alongside a writer is exactly what SQLite does well: it sees the last
    /// committed state, which `/health` has always demonstrated by answering in 0.02s during a 51s
    /// write, purely because it never touched the session.
    ///
    /// The cost is one `open` per read (tens of microseconds), which buys reads that do not care how
    /// long a write takes.
    pub(crate) async fn with_read<T, F>(
        &self,
        tool: &'static str,
        reader: crate::store::audience::Reader,
        f: F,
    ) -> std::result::Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&Session) -> crate::error::Result<T> + Send + 'static,
    {
        let paths = self.paths.clone();
        let cfg = self.cfg.clone();
        let encoder = Arc::clone(&self.encoder);
        let started = std::time::Instant::now();
        let outcome = tokio::task::spawn_blocking(move || {
            let session =
                Session::read_only_as(&paths, cfg, encoder, reader).map_err(|e| e.to_string())?;
            f(&session).map_err(|e| e.to_string())
        })
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(Ok(value)) => {
                tracing::debug!(tool, elapsed_ms, "read ok");
                Ok(value)
            }
            Ok(Err(err)) => {
                tracing::warn!(tool, elapsed_ms, error = %err, "read error");
                Err(err)
            }
            Err(join) => {
                let err = format!("memory read task failed: {join}");
                tracing::error!(tool, elapsed_ms, error = %err, "read task failed");
                Err(err)
            }
        }
    }

    /// Count a read (`visits`) without ever waiting for the store.
    ///
    /// `visits` is telemetry, not a fact, and a read must not queue behind a write to bump a counter.
    /// So this takes the writer only if it is free and gives up otherwise: a missed visit count costs
    /// nothing, while a read that waits out a rebuild is the whole bug this split exists to remove.
    /// Returns the new count when it landed, so the caller can report it exactly as before.
    pub(crate) fn bump_visit(&self, key: &str) -> Option<i64> {
        let session = self.session.try_lock().ok()?;
        session.record_visit(key).ok()
    }
}

/// One JSON object as the tool's text result.
pub(crate) fn json_ok(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// Log one `memory_search` round trip: the request, every returned hit (key/tags/distance) and
/// the size of the serialized response. Visible with `RUST_LOG=ellmephant=info`; the Docker image
/// sets that by default.
#[allow(clippy::too_many_arguments)] // one structured log line; splitting it would hurt grepping
fn log_search(
    query: Option<&str>,
    expand: u8,
    bundle: &Bundle,
    response_bytes: usize,
    elapsed_ms: u64,
) {
    // The leg counts are the point of the line: `hits` alone cannot distinguish "ten agreements"
    // from "ten word matches after the phrase leg returned nothing".
    let semantic_candidates = bundle.search.as_ref().map(|m| m.semantic_candidates);
    let lexical_candidates = bundle.search.as_ref().map(|m| m.lexical_candidates);
    tracing::info!(
        query = query.unwrap_or("-"),
        expand,
        hits = bundle.hits.len(),
        neighbors = bundle.neighbors.len(),
        ?semantic_candidates,
        ?lexical_candidates,
        response_bytes,
        elapsed_ms,
        "memory_search"
    );
    for hit in &bundle.hits {
        tracing::info!(
            key = %hit.key,
            tags = ?hit.tags,
            via = hit.via.name(),
            distance = ?hit.distance,
            "  hit"
        );
    }
}

/// The store lock, with a poisoned mutex a plain error rather than a panic.
///
/// `with_store` reaches the same lock through `spawn_blocking`; the encode worker takes it directly,
/// because it must hold it for two short pieces with a model call *between* them — which one blocking
/// closure cannot express.
#[cfg(feature = "encoder")]
fn lock(session: &Arc<Mutex<Session>>) -> crate::error::Result<std::sync::MutexGuard<'_, Session>> {
    session
        .lock()
        .map_err(|_| Error::Config("memory store lock poisoned".into()))
}

/// One pass: the head of the queue, embedded, then indexed. Returns whether it did a job.
///
/// The lock is taken twice and held across *neither* half of the model call — that is the entire
/// design; everything else here is bookkeeping around it. It also **says what it did**: a worker that
/// only logs its failures is indistinguishable from one that is not running, which is the difference
/// between a quiet store and a stalled one.
#[cfg(feature = "encoder")]
fn pass(
    session: &Arc<Mutex<Session>>,
    watch: &crate::http::LockWatch,
    encoder: &super::store::session::Encoder,
    idle: &rusqlite::Connection,
    skipped: &mut std::collections::HashMap<String, std::time::Instant>,
) -> crate::error::Result<bool> {
    // 0. Forget skips that have expired: a document that failed once must be tried again, or a
    //    transient model error would cost it its freshness forever.
    skipped.retain(|_, at| at.elapsed().as_secs() < SKIP_SECS);
    // 1. The head, on a read-only connection: no store lock, so an empty queue costs one indexed
    //    SELECT and contends with no one.
    let skip_now: std::collections::HashSet<String> = skipped.keys().cloned().collect();
    let entry = match crate::store::encode::next(idle, &skip_now)? {
        Some(entry) => entry,
        None => {
            // The refill rule, and the only producer this queue has: nothing is queued, so ask what
            // has no vector and queue it at bulk priority. A rebuild is therefore "delete the
            // vectors" plus this; a crash, a dropped entry and a hand-deleted vector are the same
            // question with the same answer, and the answer is empty when there is nothing to do.
            let skip: std::collections::HashSet<String> = skipped.keys().cloned().collect();
            let queued = {
                let _held = watch.held("memory_encode_refill");
                let session = lock(session)?;
                session.refill(&skip)?
            };
            if queued > 0 {
                tracing::info!(
                    queued,
                    "encode worker: refilled the queue from what has no vector"
                );
            }
            match crate::store::encode::next(idle, &skip_now)? {
                Some(entry) => entry,
                None => return Ok(false),
            }
        }
    };
    let started = std::time::Instant::now();
    // 2. Claim under a short lock — witnessed, so `/health` can say who holds the writer. The TEXT
    //    crosses in the claim itself: the queue carries it, so nothing is re-rendered here.
    let claim = {
        let _held = watch.held("memory_encode");
        let session = lock(session)?;
        session.claim_encode(&entry.keyslug)?
    };
    let Some(claim) = claim else {
        return Ok(false);
    };
    // 3. The model call, with the store lock held NOWHERE. This is where the 1.2 GB cold load lands:
    //    on this worker, not on whoever is waiting to write.
    let captured = claim.doc().to_string();
    let vector = match encoder.with(|e| e.embed_one(&captured)) {
        Ok(vector) => vector,
        Err(err) => {
            // KEEP the entry. It is the record that this document's vector is owed: deleting it
            // would leave a row whose vector describes text that is no longer there, with nothing
            // left to say so — `missing()` could not see it either, because a vector exists. So the
            // cost of a failure is a backlog that does not drain, which is visible; the skip list
            // only stops the retry from becoming a hot loop, and it lives in memory so a restart
            // tries again immediately.
            skipped.insert(entry.keyslug.clone(), std::time::Instant::now());
            tracing::warn!(
                keyslug = %entry.keyslug,
                error = %err,
                skip_secs = SKIP_SECS,
                "encode worker: embed failed — entry dropped, retried after the skip window"
            );
            return Ok(true);
        }
    };
    // 4. Settle under a second short lock, and read the queue's new shape while we are in there — the
    //    log line then costs no second lock, and the number it prints is the one the settle left.
    let counts = {
        let _held = watch.held("memory_encode_write");
        let session = lock(session)?;
        session.settle_encode(&claim, &vector)?;
        session.encode_counts()?
    };
    skipped.remove(&entry.keyslug);
    tracing::info!(
        keyslug = %entry.keyslug,
        elapsed_ms = started.elapsed().as_millis() as u64,
        pending = counts.pending,
        "encode worker: encoded"
    );
    Ok(true)
}

/// How long a document that failed to embed is left out of the refill rule. Long enough that a broken
/// document cannot spin (a pass is ~one embed), short enough that a transient failure recovers.
#[cfg(feature = "encoder")]
const SKIP_SECS: u64 = 600;

/// The encode worker: the queue's one consumer, and the reason a write does not embed.
///
/// A plain OS thread rather than a tokio task, deliberately. A pass is blocking SQLite calls with a
/// model call between them, and the whole point is that the model call holds the store lock nowhere —
/// which a thread says by dropping a guard, where `spawn_blocking`'s `'static` closures would make it
/// awkward to keep the connection across the call.
#[cfg(feature = "encoder")]
pub struct EncodeWorker {
    /// Set to ask the thread to finish its pass and exit.
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "encoder")]
impl EncodeWorker {
    /// Ask the worker to finish its current pass and exit.
    pub fn stop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(feature = "encoder")]
impl Drop for EncodeWorker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(feature = "encoder")]
impl MemoryServer {
    /// Spawn the one encode worker for this store.
    ///
    /// **Exactly one per process**: call it from `serve`, never from a clone — `http_router` clones
    /// the server per MCP session, so spawning in the constructor would mean one worker per client,
    /// all fighting for the single writer.
    pub fn spawn_encode_worker(&self, interval: std::time::Duration) -> EncodeWorker {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let session = Arc::clone(&self.session);
        let watch = Arc::clone(&self.watch);
        let encoder = Arc::clone(&self.encoder);
        let paths = self.paths.clone();
        let thread = std::thread::Builder::new()
            .name("encode-worker".into())
            .spawn(move || {
                // One read-only connection for the worker's life: the idle check must not take the
                // store lock, or an idle store would contend with its own readers for nothing.
                let idle = match crate::db::open_read_only(&paths) {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!(error = %err, "encode worker: cannot read the store");
                        return;
                    }
                };
                tracing::info!(
                    interval_ms = interval.as_millis() as u64,
                    "encode worker up"
                );
                let mut skipped: std::collections::HashMap<String, std::time::Instant> =
                    std::collections::HashMap::new();
                // ★ A STALE INDEX HAS NO WORK IT MAY DO (soma/ellmephant#76): the rebuild was refused or
                // has not run, so every vector written now would mix models. Hold the whole queue,
                // say so once, and wait for a human decision instead of burning an hour into a
                // poisoned index.
                let mut held_stale = false;
                while !flag.load(Ordering::SeqCst) {
                    let stale = crate::index::vec::projection_stale(&idle).unwrap_or(false);
                    if stale {
                        if !held_stale {
                            tracing::warn!(
                                "encode worker: index STALE — holding every job; a rebuild was \
                                 refused or not run (see `rebuild_max_documents`)"
                            );
                            held_stale = true;
                        }
                        std::thread::sleep(interval);
                        continue;
                    }
                    held_stale = false;
                    match pass(&session, &watch, &encoder, &idle, &mut skipped) {
                        Ok(true) => {
                            // The last owed vector closes the rebuild's books: estimate, actual.
                            if crate::store::encode::pending(&idle).unwrap_or(0) == 0 {
                                let outcome = {
                                    let _held = watch.held("memory_encode_write");
                                    match lock(&session) {
                                        Ok(s) => s.note_rebuild_finished(),
                                        Err(err) => Err(err),
                                    }
                                };
                                if let Err(err) = outcome {
                                    tracing::warn!(error = %err, "encode worker: cannot close the rebuild's books");
                                }
                            }
                        }
                        Ok(false) => {}
                        Err(err) => tracing::warn!(error = %err, "encode worker: pass failed"),
                    }
                    std::thread::sleep(interval);
                }
            })
            .map_err(|err| tracing::error!(error = %err, "encode worker: cannot spawn"))
            .ok();
        EncodeWorker { stop, thread }
    }
}

/// Deliberately **not** gated on the encoder: a store-only build has no worker to leave the work to,
/// so this is the only way it could index anything — and it should fail with the stub embedder's own
/// error, not fail to compile.
impl MemoryServer {
    /// Embed one queued document now, instead of leaving it to the worker: the `?wait=true` door.
    ///
    /// The worker's job, run on the caller's thread and under the caller's lock — which is the whole
    /// difference, and the reason this exists in exactly one place. See `ENCODE-WORKER.md`.
    pub async fn encode_now(&self, key: &str) -> std::result::Result<bool, String> {
        let key = key.to_string();
        self.with_store("memory_encode_now", move |s| s.encode_document(&key))
            .await
    }
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
        for tool in CORE_TOOLS {
            assert!(TOOLS.contains(tool), "core names an unknown tool: {tool}");
            assert!(seen.insert(*tool), "core names a tool twice: {tool}");
        }
        // The split has to be a SPLIT: a core list equal to the whole surface saves nothing.
        assert!(
            CORE_TOOLS.len() < TOOLS.len() / 2,
            "core is {} of {} — not a hot path",
            CORE_TOOLS.len(),
            TOOLS.len()
        );
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

        // Read tools declare themselves read-only.
        for name in [
            "memory_search",
            "memory_get",
            "memory_backlinks",
            "memory_stats",
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
use context::{pen_name, reader_of};

pub use transport::*;
// Params are re-exported where a caller already named them (`mcp::SearchParams`).
pub use params::*;
