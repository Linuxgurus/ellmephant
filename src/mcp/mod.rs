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

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::config::{Config, Paths};
use crate::store::session::Session;
// NOTE: do *not* import `crate::error::Result` here — `#[tool_handler]` expands to code that
// uses the unqualified `Result` from `rmcp`, and our single-parameter alias would shadow it.
use crate::error::Error;
use crate::model;
use crate::search::{Bundle, Field, Fields, MatchMode, Query, to_wire_json};
use crate::store::links::backlinks_to_wire_json;
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
enum OwnRowMiss {
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

#[tool_router]
impl MemoryServer {
    /// WHO this connection speaks for — the name a write would be stamped with, and whether it was
    /// PROVEN. A read: it changes nothing, and it never returns the secret.
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
            // A claim that did not verify still deserves an answer: the caller learns the name it
            // sent is not the name it has, which is the whole confusion this tool exists to end.
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

    /// WHICH rows have no owner — the list behind `memory_stats`' two counts.
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
        // An ownerless row is a row, so the audience tier applies exactly as it does to a search hit.
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        let limit = p.limit.unwrap_or(50).clamp(1, 500);
        let rows = self
            .with_read("memory_unowned", reader, move |s| s.unowned(limit))
            .await?;
        Ok(json_ok(
            serde_json::to_value(&rows).map_err(|e| e.to_string())?,
        ))
    }

    /// WHO a pen is, and what it is to THIS connection — the question a name alone cannot answer.
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
        // An unproven reader may still ask WHO a pen is — the tree is configuration, not content —
        // it simply gets no walk, because it has not proved to be anybody.
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

    /// CREATE a pen under this one — the runtime door (James, 2026-10-23).
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
        // A mint is a WRITE: it needs a pen, and the pen is the parent.
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

    /// ROTATE a credential — the caller's own, or a descendant's (2026-10-30).
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

    /// LEND this pen's authority DOWN to a descendant — the thing that lets a finger act.
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

    /// BORROW a pen's authority — bounded by policy, expiring, revocable.
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

    /// PASS a live session down to a descendant.
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

    /// END a session, and everything passed down from it.
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

    /// WHO HOLDS WHAT, UNTIL WHEN — the question an agent asks before it acts on borrowed authority.
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

    /// Semantic phrase search across every document type, with the graveyard behind a flag.
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

    /// The `semantic_search` body, shared by the tool and `GET /semantic` so the two doors cannot drift.
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
        // A live search: one type if asked, otherwise the whole store. Today only memories are
        // enqueued, so a request for another type must SAY that rather than return a quiet empty.
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
            // ★ `semantic_search` is the vector door and does not expose `semantic_only`: not
            // because the knob is dangerous (it can only *remove* the lexical refill, never add
            // one) but because changing this tool's default answer for existing callers is its own
            // change. Today's refill is VISIBLE through `via` and `search.semantic_candidates`, so
            // turning it off is an opt-in that can wait.
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

    /// Semantic phrase search: KNN over the document embeddings.
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

    /// Run a search and render its wire JSON. Shared by the MCP tool and `GET /search`, so the two
    /// doors emit byte-identical bodies (HTTP.md § "the wire grammar").
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
        // Default: `key` only (lean). Named fields are added, in canonical order.
        let fields = Fields::selection(p.fields.into_iter().map(FieldParam::to_field));
        // Keep a copy for the log line: the original moves into the store closure.
        let log_query = query.clone();
        let started = std::time::Instant::now();
        let bundle = self
            .with_read("memory_search", reader.clone(), move |s| {
                let q = Query {
                    text: query,
                    pattern,
                    mode,
                    k: k.unwrap_or_else(|| s.default_k()),
                    // Fall back to the configured ceiling so a bare semantic search is capped.
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
        let json = to_wire_json(&bundle, &fields).map_err(|e| e.to_string())?;
        log_search(
            log_query.as_deref(),
            expand,
            &bundle,
            json.len(),
            started.elapsed().as_millis() as u64,
        );
        Ok(json)
    }

    /// Fetch one memory in full by key.
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
        // A VERSION is reached by address, never resolved through the live row (which need not exist
        // — its history outlives it), and it counts no visit: `visits` measures the memory, not a
        // state the memory held.
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
        // The relation is computed INSIDE the read, where the session's proven pen is in hand: what
        // the owner is to the ASKER, not a property of the row.
        let (mut mem, relation) = match self
            .with_read("memory_get", reader.clone(), move |s| {
                let mem = s.show(&lookup)?;
                let relation = s.owner_relation(&mem);
                Ok((mem, relation))
            })
            .await
        {
            Ok(found) => found,
            Err(e) => return Err(self.explain_miss(&key, e, reader.clone()).await),
        };
        // Count the read best-effort: `visits` is telemetry, so a read must never wait for it. If a
        // write holds the store this visit simply goes uncounted — which is nothing, next to a read
        // that queued behind the entire write.
        if let Some(seen) = self.bump_visit(&key) {
            mem.visits = seen;
        }
        tracing::debug!(tool = "memory_get", key = %key, "memory fetched");
        // Carry the firing cue in the payload, not only the static description: the model reads
        // the returned body here, so this is where the upvote ask must live.
        let mut v = serde_json::to_value(&mem).map_err(|e| e.to_string())?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("_nudge".to_string(), serde_json::json!(GET_NUDGE));
            if let Some(walk) = relation {
                obj.insert("owner_relation".to_string(), serde_json::json!(walk));
            }
        }
        serde_json::to_string(&v).map_err(|e| e.to_string())
    }

    /// List a memory's versions, or the index of every key that holds one.
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

    /// Bump the `times_useful` counter on a memory that helped the task.
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
        pen_name(&ctx, self.declared.as_deref(), &self.paths)?; // writes are signed; anonymous is read-only
        let reader = reader_of(&ctx, self.declared.as_deref(), &self.paths);
        upvote_sweep(self, reader, p.keys.into_vec()).await
    }

    /// List the memories whose bodies link to `key`.
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

    /// Name every broken `[[…]]` edge — the finder `memory_stats.dangling` never had.
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
        Ok(crate::store::links::dangling_to_wire_json(&edges))
    }

    /// Read-only digest: how much is stored, how it is tagged and linked, and what earned its keep.
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

    /// Find near-duplicate candidates: memory pairs closer than a tight distance.
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
            return store::dupes::plan_to_wire_json(&plan).map_err(|e| e.to_string());
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
        store::dupes::to_wire_json(&pairs).map_err(|e| e.to_string())
    }

    /// Recompute every vector and rebuild the derived index — the only path out of a model or
    /// dimension change, which [`crate::index::vec::ensure`] deliberately refuses to absorb.
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
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?; // writes are signed; anonymous is read-only
        let report = self
            .with_store("memory_reembed", move |s| {
                // ★ NOT GATED, AND THAT IS THE DECISION (#168 review): a rebuild is derived data —
                // idempotent, and its output depends on the bodies, not on the news. A pen locked out
                // of an incident may still repair the index; making maintenance mean "acting" would
                // dilute the class until someone weakens it to stop the noise.
                s.reembed()
            })
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
        // `Report` is not Serialize; spell the fields out so the response stays explicit.
        Ok(json_ok(serde_json::json!({
            "status": "queued",
            "queued": report.queued,
            "dim": report.dim,
            "rebuilt": report.rebuilt,
            "model": report.model,
        })))
    }

    /// Rebuild the `[[link]]` graph from every body. Model-free and seconds long.
    ///
    /// `memory_link` is derived, but its edges are computed at *write* time, so a body rewritten by
    /// a path that does not re-derive leaks a stale edge: a rename or merge `replace()`, a parser
    /// change, a binary that predates the current one. `memory_reembed` repairs it as a side effect
    /// at the cost of a model load and an hour of worker time, so small drift went unrepaired and
    /// `stats.dangling` accused the writer for edits that were already correct.
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
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?; // writes are signed; anonymous is read-only
        let report = self
            .with_store("memory_reparse", move |s| {
                // ★ NOT GATED (#168 review): the link graph is derived data. Rebuilding it is
                // idempotent and changes nothing anyone was told, so a locked pen may do it.
                s.reparse()
            })
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
        let pen = caller_of(&ctx, self.declared.as_deref(), &self.paths)
            .pen()
            .map_err(|r| r.to_string())?
            .clone(); // writes are signed; anonymous is read-only
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
        project::view_to_wire_json(&view).map_err(|e| e.to_string())
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
        project::list_to_wire_json(&rows).map_err(|e| e.to_string())
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
        let proven = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let sender = sender.trim();
        let sender = if sender.is_empty() {
            proven.clone()
        } else {
            sender.to_string()
        };
        let (log_sender, log_recipient) = (sender.clone(), recipient.clone());
        let out = self
            .with_store("mail_send", move |s| {
                s.mail_send(
                    &proven,
                    &sender,
                    &recipient,
                    subject.as_deref().unwrap_or(""),
                    &body,
                    encoding.as_deref().unwrap_or("text"),
                )
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
        serde_json::to_string(&out).map_err(|e| e.to_string())
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
        let caller = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        if !caller.eq_ignore_ascii_case(p.grantor.trim()) {
            return Err(format!(
                "refused: `{caller}` may grant its OWN name only; `{}` must grant itself",
                p.grantor
            ));
        }
        let grantor = p.grantor;
        let surrogate = p.surrogate;
        let scopes = p.scopes.unwrap_or_else(|| "send".to_string());
        let expires_at = p.expires_at;
        let note = p.note;
        let out = self
            .with_store("mail_surrogate_grant", move |s| {
                s.mail_grant(
                    &grantor,
                    &surrogate,
                    &scopes,
                    expires_at.as_deref(),
                    note.as_deref(),
                )
            })
            .await?;
        Ok(json_ok(
            serde_json::to_value(&out).map_err(|e| e.to_string())?,
        ))
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
        let caller = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        if !caller.eq_ignore_ascii_case(p.grantor.trim()) {
            return Err(format!(
                "refused: `{caller}` may revoke its OWN grants only"
            ));
        }
        let (grantor, surrogate) = (p.grantor, p.surrogate);
        let n = self
            .with_store("mail_surrogate_revoke", move |s| {
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
        let caller = pen_name(&ctx, self.declared.as_deref(), &self.paths)?;
        let subject = p.surrogate.unwrap_or(caller);
        let rows = self
            .with_read(
                "mail_surrogate_list",
                reader_of(&ctx, self.declared.as_deref(), &self.paths),
                move |s| s.mail_surrogates_of(&subject),
            )
            .await?;
        Ok(json_ok(
            serde_json::to_value(&rows).map_err(|e| e.to_string())?,
        ))
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
                .with_read(
                    "mail_inbox",
                    crate::store::audience::Reader::everything(),
                    move |s| {
                        // Three facts, and they were being conflated: the top of THIS PAGE, the top of
                        // the BOX, and where the READER is. `max_seq` was read as "newest letter that
                        // exists" while it meant "highest seq returned" — which cost two readers an
                        // hour of theorising about a 273-letter discrepancy that never existed
                        // (measured 2026-10-21).
                        let stored = s.mail_cursor(&me, &who)?;
                        let seq =
                            since.unwrap_or_else(|| stored.as_ref().map(|c| c.seq).unwrap_or(0));
                        Ok((s.mail_inbox(&who, seq, limit)?, s.mail_head(&who)?, stored))
                    },
                )
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
        let (log_reader, log_recipient) = (reader.clone(), recipient.clone());
        let out = self
            .with_store("mail_ack", move |s| {
                s.mail_advance(&reader, &recipient, seq)
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
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// Turn a bare "no such memory" into a claim with its evidence (soma/ellmephant#57).
    ///
    /// A miss says something about the WHOLE store, and on 2026-10-21 it said the wrong thing for 42
    /// minutes while nobody could test which of four hypotheses was true. Nothing here changes the
    /// answer — a missing row is still an error — but it names the store, its size, its schema and the
    /// key's history, so the next reader can tell an absent key from a wrong file, a stale reader or a
    /// delete. Refusals that are NOT misses pass through untouched.
    async fn explain_miss(
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
    fn own_row_miss(
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
            .with_read(
                "mail_read",
                crate::store::audience::Reader::everything(),
                move |s| s.mail_read(seq),
            )
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
        pen_name(&ctx, self.declared.as_deref(), &self.paths)?; // writes are signed; anonymous is read-only
        let keep_days = p.keep_days;
        let removed = self
            .with_store("mail_prune", move |s| s.mail_prune(keep_days))
            .await?;
        tracing::info!(tool = "mail_prune", keep_days, removed, "letters pruned");
        Ok(serde_json::json!({ "removed": removed, "keep_days": keep_days }).to_string())
    }

    /// Every channel, its policy, and — for a proven pen — what it has not listened to yet.
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

    /// Say one thing in one channel.
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

    /// The channel log, without moving a receipt.
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
        pen_name(&ctx, self.declared.as_deref(), &self.paths)?; // ★ A CHANNEL LOG IS NOT PUBLIC (#158 review)
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

    /// LISTEN: take the unread messages and mark them fetched — the only call that clears a gate.
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

    /// Mark that a turn boundary OFFERED this pen its inbox.
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

    /// What, if anything, is stopping this pen from changing the record.
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

    /// Join a channel.
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

    /// Invite a pen to a channel.
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

    /// Leave a channel.
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

    /// Wait for channel messages to arrive — the push primitive a body blocks on.
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
        // ★ NO CURSOR MEANS "FROM NOW", NOT "FROM THE BEGINNING OF TIME" (#168 review): a fresh body
        // that waits should see what arrives next, not the entire backlog.
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

    /// WHO is in a channel — effective membership, not the override rows.
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
        pen_name(&ctx, self.declared.as_deref(), &self.paths)?; // the roster is configuration, not a public list
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

    /// Create a channel.
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

    /// Change a gate, and record who did it.
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

    /// The recorded changes to a channel.
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
        pen_name(&ctx, self.declared.as_deref(), &self.paths)?; // who gated what is not a public record
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

    /// Set how long a channel keeps its text.
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

    /// Expire the text past each channel's retention. Receipts stay.
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
        let pen = pen_name(&ctx, self.declared.as_deref(), &self.paths)?; // a prune writes; anonymous is read-only
        let only = p.channel;
        let rows = self
            .with_store("channel_prune", move |s| {
                s.channel_gate(&pen, crate::store::channel::Class::Acting)?;
                s.channel_prune(only.as_deref())
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

#[tool_handler]
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
mod transport;
use context::{caller_of, pen_name, reader_of};

pub use transport::*;
// Params are re-exported where a caller already named them (`mcp::SearchParams`).
pub use params::*;
