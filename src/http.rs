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
use crate::store::{links, project, tags};
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
    fn witness(&self) -> String {
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

/// Everything `/health` needs, without a `Session`: the store path, the writer witness and the
/// process start time.
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
}

impl HttpState {
    /// Assemble the surface state.
    pub fn new(paths: Paths, cfg: Config, watch: Arc<LockWatch>, started: Instant) -> Self {
        Self {
            paths,
            cfg,
            watch,
            started,
        }
    }
}

/// The health of the store, rendered either as the prose kubectl reads or as JSON.
struct HealthReport {
    version: &'static str,
    uptime_s: u64,
    schema: i32,
    latest: i32,
    memories: i64,
    vectors: i64,
    projection: String,
    links: i64,
    links_dangling: usize,
    tags: usize,
    projects: i64,
    projects_dangling: usize,
    writer: String,
    ready: bool,
    reason: Option<String>,
}

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
        })
    }
}

/// Read-only readiness for a probe: schema, projection, vectors, the derived counts and the
/// writer witness. Never migrates, never takes the store lock.
fn probe(paths: &Paths, watch: &LockWatch, started: Instant) -> HealthReport {
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
        ready: false,
        reason: None,
    };
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
    report.ready = report.schema == latest && !stale && report.vectors >= report.memories;
    if !report.ready {
        report.reason = Some(if report.schema != latest {
            format!("schema v{} behind binary v{latest}", report.schema)
        } else if stale {
            "projection STALE — run `reembed`".to_string()
        } else {
            format!(
                "{} memory row(s) without a vector — run `reembed`",
                report.memories - report.vectors
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
