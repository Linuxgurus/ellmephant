//! The server itself: the shared session, the two store handles, and the encode worker.
//!
//! Split out of `mcp/mod.rs` (soma/ellmephant#50). The FIELDS are `pub(super)` because the tool
//! groups are SIBLING modules of this one and read `self.paths` / `self.declared` directly; a
//! getter per field would be noise, and a private field would make the tools unable to compile.

use super::*;
use crate::mcp::context::{caller_of, pen_name, reader_of};

/// The MCP server. Holds the single store [`Session`] behind a mutex; `#[tool_handler]`
/// reaches the tool table through the generated `Self::tool_router()`, so there is no field.
///
/// (This comment was found attached to `ChannelSendParams` a hundred lines away — two lines about
/// the server had drifted onto a params struct in an earlier copy-edit. It is back where it belongs.)
#[derive(Clone)]
pub struct MemoryServer {
    pub(super) session: Arc<Mutex<Session>>,
    pub(super) watch: Arc<crate::http::LockWatch>,
    /// Read-side handholds, taken once at construction so that a READ never has to touch `session`:
    /// the writer holds that for the whole of a write, embedding included, and a reader queueing on
    /// it is precisely the stall this split removes.
    pub(super) paths: crate::config::Paths,
    pub(super) cfg: crate::config::Config,
    pub(super) encoder: Arc<super::store::session::Encoder>,
    /// ★ A DECLARED PEN, for transports that carry no HTTP request (stdio, in-process). A server
    /// started BY a process is as trusted as that process, so it may say who it is once, at
    /// construction; every write it makes is stamped with that name instead of being refused as
    /// anonymous. HTTP transports ignore this — their headers decide.
    pub(super) declared: Option<String>,
    /// Which tools this instance advertises. One process serves both doors; the value is set once
    /// at mount time and never changes.
    pub(super) surface: Surface,
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
    pub(super) async fn mail_box_for(
        &self,
        ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
        requested: &str,
        scope: &'static str,
    ) -> std::result::Result<String, String> {
        let who = pen_name(ctx, self.declared.as_deref(), &self.paths)?;
        let (reader, box_name) = (who.clone(), requested.to_string());
        // ★ ONE RULE, ONE SENTENCE (soma/ellmephant#235). This used to ask `mail_may_ack` or
        // `mail_may_read` itself and then compose the refusal — in a string a copy-edit had left
        // with a run of blanks where a word belonged. The rule and its sentence live in the store
        // now, so HTTP cannot answer a different question.
        let scope = if scope == "ack" {
            crate::store::mail::BoxScope::Ack
        } else {
            crate::store::mail::BoxScope::Read
        };
        self.with_read(
            "mail_box_for",
            reader_of(ctx, self.declared.as_deref(), &self.paths),
            move |s| s.mail_may_open_box(&reader, &box_name, scope),
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(who)
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
    pub(crate) async fn with_store<T, F>(&self, tool: &'static str, f: F) -> crate::error::Result<T>
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
                .map_err(|_| crate::error::Error::Internal("memory store lock poisoned".into()))?;
            tracing::debug!(
                tool,
                wait_ms = wait.elapsed().as_millis() as u64,
                "store lock acquired"
            );
            // Witness the holder for exactly as long as the lock is held.
            let _held = watch.held(tool);
            f(&mut guard)
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
                let err =
                    crate::error::Error::Internal(format!("memory store task failed: {join}"));
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
    ) -> crate::error::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Session) -> crate::error::Result<T> + Send + 'static,
    {
        let paths = self.paths.clone();
        let cfg = self.cfg.clone();
        let encoder = Arc::clone(&self.encoder);
        let started = std::time::Instant::now();
        let outcome = tokio::task::spawn_blocking(move || {
            let session = Session::read_only_as(&paths, cfg, encoder, reader)?;
            f(&session)
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
                let err = crate::error::Error::Internal(format!("memory read task failed: {join}"));
                tracing::error!(tool, elapsed_ms, error = %err, "read task failed");
                Err(err)
            }
        }
    }

    /// ★ THE WRITE ENVELOPE (soma/ellmephant#231, #237).
    ///
    /// Every write tool used to open with the same five lines — borrow the pen, open the store, gate,
    /// serialize, log — written out by hand 42 times. The gate is NOT a parameter here on purpose:
    /// #234 moved every `channel_gate` inside `ops::`, so the gate travels with the VERB, not with
    /// the door. A helper that accepted one would be a second place to forget it.
    ///
    /// Returns the outcome so the body can log its own fields or branch on it (an upsert conflict is
    /// a result, not an error, and only the verb knows that). [`Self::write_tool`] is the common tail.
    pub(crate) async fn write_outcome<T, F>(
        &self,
        name: &'static str,
        ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
        f: F,
    ) -> std::result::Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session, &crate::names::Pen) -> crate::error::Result<T> + Send + 'static,
    {
        let pen = caller_of(ctx, self.declared.as_deref(), &self.paths)
            .pen()
            .map_err(|r| r.to_string())?
            .clone();
        let actor = pen.clone();
        self.with_store(name, move |s| f(s, &actor))
            .await
            .map_err(|e| e.to_string())
    }

    /// The write envelope, ending in the one serialization a tool result is allowed.
    ///
    /// A body that calls this is the closure only: no pen, no store handle, no `to_string`.
    pub(crate) async fn write_tool<T, F>(
        &self,
        name: &'static str,
        ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
        f: F,
    ) -> std::result::Result<String, String>
    where
        T: serde::Serialize + Send + 'static,
        F: FnOnce(&mut Session, &crate::names::Pen) -> crate::error::Result<T> + Send + 'static,
    {
        let out = self.write_outcome(name, ctx, f).await?;
        serde_json::to_string(&out).map_err(|e| e.to_string())
    }

    /// ★ THE READ ENVELOPE. A read is scoped by who is asking and runs on its own connection, so a
    /// reader never queues behind a write; the body supplies only the question.
    pub(crate) async fn read_outcome<T, F>(
        &self,
        name: &'static str,
        ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
        f: F,
    ) -> std::result::Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&Session) -> crate::error::Result<T> + Send + 'static,
    {
        let reader = reader_of(ctx, self.declared.as_deref(), &self.paths);
        self.with_read(name, reader, f)
            .await
            .map_err(|e| e.to_string())
    }

    /// The read envelope, ending in the one serialization a tool result is allowed.
    pub(crate) async fn read_tool<T, F>(
        &self,
        name: &'static str,
        ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
        f: F,
    ) -> std::result::Result<String, String>
    where
        T: serde::Serialize + Send + 'static,
        F: FnOnce(&Session) -> crate::error::Result<T> + Send + 'static,
    {
        let out = self.read_outcome(name, ctx, f).await?;
        serde_json::to_string(&out).map_err(|e| e.to_string())
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

/// Log one `memory_search` round trip: the request, every returned hit (key/tags/distance) and
/// the size of the serialized response. Visible with `RUST_LOG=ellmephant=info`; the Docker image
/// sets that by default.
#[allow(clippy::too_many_arguments)] // one structured log line; splitting it would hurt grepping
pub(crate) fn log_search(
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
    pub async fn encode_now(&self, key: &str) -> crate::error::Result<bool> {
        let key = key.to_string();
        self.with_store("memory_encode_now", move |s| s.encode_document(&key))
            .await
    }
}
