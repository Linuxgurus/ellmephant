//! The plain-HTTP surface: reads that need no MCP session (see HTTP.md).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use rusqlite::{Connection, OpenFlags};

use crate::config::{Config, Paths};
use crate::store::{encode, links, project, tags};
use crate::{db, index};

/// Epoch milliseconds; 0 on a clock that has run backwards.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Records which tool currently holds the store lock, and since when. `/health` reads it from
/// outside the process; the reader never waits on the holder, only on the brief set/clear.
#[derive(Default)]
pub struct LockWatch {
    held_since_ms: Arc<AtomicU64>,
    tool: Arc<Mutex<Option<&'static str>>>,
}

impl LockWatch {
    /// A free watch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the store busy with `tool` until the returned guard drops.
    pub fn held(&self, tool: &'static str) -> LockGuard {
        *self.lock_tool() = Some(tool);
        self.held_since_ms.store(now_ms(), Ordering::SeqCst);
        LockGuard {
            held_since_ms: Arc::clone(&self.held_since_ms),
            tool: Arc::clone(&self.tool),
        }
    }

    /// How long the current holder has held the lock, or `None` when free.
    pub fn busy_ms(&self) -> Option<u64> {
        let since = self.held_since_ms.load(Ordering::SeqCst);
        (since != 0).then(|| now_ms().saturating_sub(since))
    }

    /// Which tool holds the lock, or `None` when free.
    pub fn holder(&self) -> Option<&'static str> {
        *self.lock_tool()
    }

    /// The one-line writer witness (`free`, or `busy 41.2s (memory_reembed)`).
    pub(crate) fn witness(&self) -> String {
        match self.busy_ms() {
            Some(ms) => match self.holder() {
                Some(tool) => format!("busy {:.1}s ({tool})", ms as f64 / 1000.0),
                None => format!("busy {:.1}s", ms as f64 / 1000.0),
            },
            None => "free".to_string(),
        }
    }

    fn lock_tool(&self) -> std::sync::MutexGuard<'_, Option<&'static str>> {
        self.tool.lock().unwrap_or_else(|e| e.into_inner())
    }
}
/// Marks the store busy for as long as it is held; dropping it clears the witness.
pub struct LockGuard {
    held_since_ms: Arc<AtomicU64>,
    tool: Arc<Mutex<Option<&'static str>>>,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        *self.tool.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.held_since_ms.store(0, Ordering::SeqCst);
    }
}

/// Everything the plain-HTTP surface needs: the store path, the writer witness, the process start
/// time, the write credential and (for the reads that reuse it) the shared MCP session.
#[derive(Clone)]
pub struct HttpState {
    /// The store this surface reads.
    pub paths: Paths,
    /// The binary's configuration (paths and the model it was built to speak).
    pub cfg: Config,
    /// The writer witness shared with the MCP server.
    pub watch: Arc<LockWatch>,
    /// When the process came up, for the uptime line.
    pub started: Instant,
    /// The bearer that may write, or `None` for "no writes". Resolved to a scope per request.
    write_token: Option<String>,
    /// The session behind `/mcp`, reached through the same `with_store` path so a read over HTTP
    /// and one over MCP are literally the same call.
    server: Option<crate::mcp::MemoryServer>,
}

impl HttpState {
    /// Assemble the surface state. The write credential comes from `ELLMEPHANT_WRITE_TOKEN`.
    pub fn new(paths: Paths, cfg: Config, watch: Arc<LockWatch>, started: Instant) -> Self {
        Self {
            paths,
            cfg,
            watch,
            started,
            write_token: std::env::var("ELLMEPHANT_WRITE_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            server: None,
        }
    }

    /// Set the write bearer (tests, or a future config source). `None` refuses every write.
    pub fn with_write_token(mut self, token: Option<String>) -> Self {
        self.write_token = token;
        self
    }

    /// Share the MCP session with the plain-HTTP reads. `/health` and `/bootstrap` never use it.
    pub fn with_server(mut self, server: crate::mcp::MemoryServer) -> Self {
        self.server = Some(server);
        self
    }

    /// The configured write bearer.
    pub fn write_token(&self) -> Option<&str> {
        self.write_token.as_deref()
    }

    /// The session shared with `/mcp`. `None` only before [`with_server`](Self::with_server).
    pub(crate) fn server(&self) -> Option<&crate::mcp::MemoryServer> {
        self.server.as_ref()
    }
}

/// The health of the store, rendered either as the prose kubectl reads or as JSON. `/bootstrap`
/// reads the same facts, so the fields are crate-visible.
pub(crate) struct HealthReport {
    pub(crate) version: &'static str,
    pub(crate) uptime_s: u64,
    pub(crate) schema: i32,
    pub(crate) latest: i32,
    pub(crate) memories: i64,
    pub(crate) vectors: i64,
    pub(crate) projection: String,
    pub(crate) links: i64,
    pub(crate) links_dangling: usize,
    pub(crate) tags: usize,
    pub(crate) projects: i64,
    pub(crate) projects_dangling: usize,
    pub(crate) writer: String,
    /// Documents still owing the vector index. A metric, never a gate: a write schedules its document
    /// and returns, so a non-empty backlog is the *normal* state of a healthy store (`ENCODE-WORKER.md`).
    pub(crate) encode_pending: i64,
    /// Queue entries the encoder has given up on after `encode::MAX_ATTEMPTS` — the one queue number
    /// that is a defect rather than a load, because the document is now silently unsearchable.
    pub(crate) encode_failed: i64,
    /// Age of the oldest pending document in seconds, `None` when the queue is empty.
    pub(crate) encode_oldest_s: Option<i64>,
    /// Age of the newest consistent snapshot (`snapshot/memory.db`) in seconds, `None` when none has
    /// been written. ★ THIS IS THE QUIESCE, MADE LEGIBLE: a backup of a live SQLite file is not
    /// atomic, so "how old is the consistent copy?" is a question a bell can answer without kubectl
    /// and without a cross-namespace grant.
    pub(crate) snapshot_age_s: Option<i64>,
    pub(crate) ready: bool,
    pub(crate) reason: Option<String>,
}

/// How long the writer may be held before `/health` stops calling the store writable. Every write is
/// milliseconds now that embedding left the write path, so this is not a latency budget — it is the
/// line between "busy" and "wedged", and the reason it exists at all is an incident where the store
/// reported `ready: true` beside `writer: busy 1239.1s`.
const WRITER_GRACE_MS: u64 = 2_000;

impl HealthReport {
    /// The report for when the probe task itself could not run.
    fn failed(reason: &str) -> Self {
        Self {
            version: crate::VERSION,
            uptime_s: 0,
            schema: 0,
            latest: db::migrate::latest(),
            memories: 0,
            vectors: 0,
            projection: "unknown".to_string(),
            links: 0,
            links_dangling: 0,
            tags: 0,
            projects: 0,
            projects_dangling: 0,
            writer: "free".to_string(),
            encode_pending: 0,
            encode_failed: 0,
            encode_oldest_s: None,
            snapshot_age_s: None,
            ready: false,
            reason: Some(reason.to_string()),
        }
    }

    /// The text form: a status line, then one aligned line per fact.
    fn to_text(&self) -> String {
        let status = if self.ready { "ready" } else { "NOT READY" };
        let mut out = format!("ellmephant {} {status}\n", self.version);
        out.push_str(&format!("  {:<12}{}s\n", "uptime", self.uptime_s));
        out.push_str(&format!(
            "  {:<12}v{} (binary v{})\n",
            "schema", self.schema, self.latest
        ));
        out.push_str(&format!("  {:<12}{}\n", "memories", self.memories));
        out.push_str(&format!(
            "  {:<12}{}/{}\n",
            "vectors", self.vectors, self.memories
        ));
        out.push_str(&format!("  {:<12}{}\n", "projection", self.projection));
        out.push_str(&format!(
            "  {:<12}{} edge(s), {} dangling\n",
            "links", self.links, self.links_dangling
        ));
        out.push_str(&format!("  {:<12}{} tag(s)\n", "tags", self.tags));
        out.push_str(&format!(
            "  {:<12}{} project(s), {} dangling preload key(s)\n",
            "projects", self.projects, self.projects_dangling
        ));
        out.push_str(&format!("  {:<12}{}\n", "writer", self.writer));
        // The queue: what the encoder owes, and whether any of it has given up. Never part of the
        // status line — a backlog is a load, not a fault. A store too old to HAVE the queue must say
        // so rather than print zeroes, which would read as an all-clear on a store we cannot read.
        let queue = if self.schema < encode::SCHEMA {
            format!(
                "not in schema v{} — the store is behind this binary",
                encode::SCHEMA
            )
        } else if let Some(age) = self.encode_oldest_s {
            format!("{} pending (oldest {}s)", self.encode_pending, age)
        } else {
            format!("{} pending", self.encode_pending)
        };
        out.push_str(&format!("  {:<12}{queue}\n", "encode"));
        let snap = match self.snapshot_age_s {
            Some(age) => format!("{age}s ago"),
            None => "none yet — the backup is copying a LIVE database".to_string(),
        };
        out.push_str(&format!("  {:<12}{snap}\n", "snapshot"));
        if let Some(reason) = &self.reason {
            out.push_str(&format!("  {:<12}false: {reason}\n", "ready"));
        }
        out
    }

    /// The stable JSON envelope — same facts as [`to_text`](Self::to_text).
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "version": self.version,
            "ready": self.ready,
            "reason": self.reason,
            "uptime_s": self.uptime_s,
            "schema": self.schema,
            "latest": self.latest,
            "memories": self.memories,
            "vectors": self.vectors,
            "projection": self.projection,
            "links": self.links,
            "links_dangling": self.links_dangling,
            "tags": self.tags,
            "projects": self.projects,
            "projects_dangling": self.projects_dangling,
            "writer": self.writer,
            // Reported, never gated on — see the field docs. `encode_oldest_s` is null when the
            // queue is empty, which is how a caller tells "empty" from "nothing reported".
            "encode_pending": self.encode_pending,
            "encode_failed": self.encode_failed,
            "encode_oldest_s": self.encode_oldest_s,
            // The quiesce: how old the newest CONSISTENT copy is. `null` means none has been written,
            // which is a fact worth surfacing rather than a zero.
            "snapshot_age_s": self.snapshot_age_s,
        })
    }
}

/// Read-only readiness for a probe: schema, projection, vectors, the derived counts and the
/// writer witness. Never migrates, never takes the store lock.
pub(crate) fn probe(paths: &Paths, watch: &LockWatch, started: Instant) -> HealthReport {
    let latest = db::migrate::latest();
    let mut report = HealthReport {
        version: crate::VERSION,
        uptime_s: started.elapsed().as_secs(),
        schema: 0,
        latest,
        memories: 0,
        vectors: 0,
        projection: "unrecorded".to_string(),
        links: 0,
        links_dangling: 0,
        tags: 0,
        projects: 0,
        projects_dangling: 0,
        writer: watch.witness(),
        encode_pending: 0,
        encode_failed: 0,
        encode_oldest_s: None,
        snapshot_age_s: None,
        ready: false,
        reason: None,
    };
    // The quiesce is a property of the DIRECTORY, not of the open database: report it even when the
    // store is missing or unreadable, because "how old is my last consistent copy?" matters most
    // exactly then.
    report.snapshot_age_s = crate::store::snapshot::age_seconds(paths.root());
    // The probe must not create the file, so a missing store short-circuits before any open.
    if !paths.db().exists() {
        report.reason = Some("store missing".to_string());
        return report;
    }
    let stale = match read_store(paths, &mut report) {
        Ok(stale) => stale,
        Err(err) => {
            report.reason = Some(err.to_string());
            return report;
        }
    };
    // Ready means READABLE **and** WRITABLE, and nothing else. A non-empty encode queue is the normal
    // state of a healthy store — a write schedules its document and returns — so it is reported above
    // and never gated on here; nor is "every row has a vector", for the same reason.
    let wedged = watch.busy_ms().is_some_and(|ms| ms > WRITER_GRACE_MS);
    report.ready = report.schema == latest && !stale && !wedged;
    if !report.ready {
        report.reason = Some(if report.schema != latest {
            format!("schema v{} behind binary v{latest}", report.schema)
        } else if stale {
            "projection STALE — run `reembed`".to_string()
        } else {
            format!(
                "writer held too long (`{}`) — a write would queue behind it",
                report.writer
            )
        });
    }
    report
}

/// Fill `report` from its own read-only connection. Returns whether the projection is stale.
fn read_store(paths: &Paths, report: &mut HealthReport) -> crate::error::Result<bool> {
    // vec0 needs the extension; register before opening, and never write a file.
    index::vec::register_extension();
    let conn = Connection::open_with_flags(paths.db(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    report.schema = db::migrate::version(&conn)?;
    report.memories = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
    report.vectors = match index::vec::stored_dim(&conn)? {
        Some(_) => index::vec::count(&conn)?,
        None => 0,
    };
    let stale = index::vec::projection_stale(&conn)?;
    report.projection = if stale {
        "STALE — run `reembed`".to_string()
    } else if db::meta::get(&conn, db::meta::PROJECTION)?.is_some() {
        "current".to_string()
    } else {
        "unrecorded".to_string()
    };
    report.links = conn.query_row("SELECT count(*) FROM memory_link", [], |r| r.get(0))?;
    report.links_dangling = links::dangling(&conn)?.len();
    report.tags = tags::list(&conn)?.len();
    report.projects = project::count(&conn)?;
    report.projects_dangling = project::dangling(&conn)?.len();
    // The queue table only exists from v8 on, and the probe's job is to explain a store it does not
    // recognise — so an older schema reports no queue rather than failing the whole probe on it.
    if report.schema >= encode::SCHEMA {
        let queue = encode::counts(&conn)?;
        report.encode_pending = queue.pending;
        report.encode_oldest_s = queue.oldest_s;
    }
    Ok(stale)
}

/// `GET /health` — read-only readiness plus the writer witness. `?format=json` for machines;
/// a not-ready store still returns its body, under 503.
pub async fn health(
    State(state): State<HttpState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let json = matches!(params.get("format").map(String::as_str), Some("json"));
    let probe_state = state.clone();
    // The probe is synchronous rusqlite; keep it off a tokio worker (house style).
    let report = tokio::task::spawn_blocking(move || {
        probe(&probe_state.paths, &probe_state.watch, probe_state.started)
    })
    .await
    .unwrap_or_else(|_| HealthReport::failed("health probe task failed"));
    let status = if report.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let (content_type, body) = if json {
        ("application/json", report.to_json().to_string())
    } else {
        ("text/plain; charset=utf-8", report.to_text())
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("health response")
}
