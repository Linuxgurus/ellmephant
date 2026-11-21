use super::*;
use crate::mcp::context::{pen_name, reader_of};

#[derive(Clone)]
pub struct MemoryServer {
    pub(super) session: Arc<Mutex<Session>>,
    pub(super) watch: Arc<crate::http::LockWatch>,
    pub(super) paths: crate::config::Paths,
    pub(super) cfg: crate::config::Config,
    pub(super) encoder: Arc<super::store::session::Encoder>,
    pub(super) declared: Option<String>,
    pub(super) surface: Surface,
}

pub(crate) enum OwnRowMiss {
    ExcludedByItsAudience(String),
    Divergence(String),
    NotYours,
}

impl MemoryServer {
    pub fn new(session: Session) -> Self {
        Self::from_shared(Arc::new(Mutex::new(session)))
    }

    pub fn from_shared(session: Arc<Mutex<Session>>) -> Self {
        Self::with_watch(session, Arc::new(crate::http::LockWatch::new()))
    }

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

    pub fn with_surface(mut self, surface: Surface) -> Self {
        self.surface = surface;
        self
    }

    pub(super) async fn mail_box_for(
        &self,
        ctx: &rmcp::service::RequestContext<rmcp::RoleServer>,
        requested: &str,
        scope: &'static str,
    ) -> std::result::Result<String, String> {
        let who = pen_name(ctx, self.declared.as_deref(), &self.paths)?;
        let (reader, box_name) = (who.clone(), requested.to_string());
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

    pub fn with_identity(mut self, name: impl Into<String>) -> Self {
        self.declared = Some(name.into());
        self
    }

    pub fn watch(&self) -> Arc<crate::http::LockWatch> {
        Arc::clone(&self.watch)
    }

    pub(crate) async fn with_store<T, F>(&self, tool: &'static str, f: F) -> crate::error::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session) -> crate::error::Result<T> + Send + 'static,
    {
        let session = Arc::clone(&self.session);
        let watch = Arc::clone(&self.watch);
        let started = std::time::Instant::now();
        let outcome = tokio::task::spawn_blocking(move || {
            let wait = std::time::Instant::now();
            let mut guard = session
                .lock()
                .map_err(|_| crate::error::Error::Internal("memory store lock poisoned".into()))?;
            tracing::debug!(
                tool,
                wait_ms = wait.elapsed().as_millis() as u64,
                "store lock acquired"
            );
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

    pub(crate) fn bump_visit(&self, key: &str) -> Option<i64> {
        let session = self.session.try_lock().ok()?;
        session.record_visit(key).ok()
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn log_search(
    query: Option<&str>,
    expand: u8,
    bundle: &Bundle,
    response_bytes: usize,
    elapsed_ms: u64,
) {
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

#[cfg(feature = "encoder")]
fn lock(session: &Arc<Mutex<Session>>) -> crate::error::Result<std::sync::MutexGuard<'_, Session>> {
    session
        .lock()
        .map_err(|_| Error::Config("memory store lock poisoned".into()))
}

#[cfg(feature = "encoder")]
fn pass(
    session: &Arc<Mutex<Session>>,
    watch: &crate::http::LockWatch,
    encoder: &super::store::session::Encoder,
    idle: &rusqlite::Connection,
    skipped: &mut std::collections::HashMap<String, std::time::Instant>,
) -> crate::error::Result<bool> {
    skipped.retain(|_, at| at.elapsed().as_secs() < SKIP_SECS);
    let skip_now: std::collections::HashSet<String> = skipped.keys().cloned().collect();
    let entry = match crate::store::encode::next(idle, &skip_now)? {
        Some(entry) => entry,
        None => {
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
    let claim = {
        let _held = watch.held("memory_encode");
        let session = lock(session)?;
        session.claim_encode(&entry.keyslug)?
    };
    let Some(claim) = claim else {
        return Ok(false);
    };
    let captured = claim.doc().to_string();
    let vector = match encoder.with(|e| e.embed_one(&captured)) {
        Ok(vector) => vector,
        Err(err) => {
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

#[cfg(feature = "encoder")]
const SKIP_SECS: u64 = 600;

#[cfg(feature = "encoder")]
pub struct EncodeWorker {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "encoder")]
impl EncodeWorker {
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

impl MemoryServer {
    pub async fn encode_now(&self, key: &str) -> crate::error::Result<bool> {
        let key = key.to_string();
        self.with_store("memory_encode_now", move |s| s.encode_document(&key))
            .await
    }
}
