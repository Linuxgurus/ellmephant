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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Default)]
pub struct LockWatch {
    held_since_ms: Arc<AtomicU64>,
    tool: Arc<Mutex<Option<&'static str>>>,
}

impl LockWatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn held(&self, tool: &'static str) -> LockGuard {
        *self.lock_tool() = Some(tool);
        self.held_since_ms.store(now_ms(), Ordering::SeqCst);
        LockGuard {
            held_since_ms: Arc::clone(&self.held_since_ms),
            tool: Arc::clone(&self.tool),
        }
    }

    pub fn busy_ms(&self) -> Option<u64> {
        let since = self.held_since_ms.load(Ordering::SeqCst);
        (since != 0).then(|| now_ms().saturating_sub(since))
    }

    pub fn holder(&self) -> Option<&'static str> {
        *self.lock_tool()
    }

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

#[derive(Clone)]
pub struct HttpState {
    pub paths: Paths,
    pub cfg: Config,
    pub watch: Arc<LockWatch>,
    pub started: Instant,
    write_token: Option<String>,
    server: Option<crate::mcp::MemoryServer>,
}

impl HttpState {
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

    pub fn with_write_token(mut self, token: Option<String>) -> Self {
        self.write_token = token;
        self
    }

    pub fn with_server(mut self, server: crate::mcp::MemoryServer) -> Self {
        self.server = Some(server);
        self
    }

    pub fn write_token(&self) -> Option<&str> {
        self.write_token.as_deref()
    }

    pub(crate) fn server(&self) -> Option<&crate::mcp::MemoryServer> {
        self.server.as_ref()
    }
}

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
    pub(crate) encode_pending: i64,
    pub(crate) encode_failed: i64,
    pub(crate) encode_oldest_s: Option<i64>,
    pub(crate) snapshot_age_s: Option<i64>,
    pub(crate) sudo_policies: i64,
    pub(crate) sudo_live: i64,
    pub(crate) ready: bool,
    pub(crate) reason: Option<String>,
}

const WRITER_GRACE_MS: u64 = 2_000;

impl HealthReport {
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
            sudo_policies: 0,
            sudo_live: 0,
            ready: false,
            reason: Some(reason.to_string()),
        }
    }

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
        out.push_str(&format!(
            "  {:<12}{} rule(s), {} session(s)\n",
            "sudo", self.sudo_policies, self.sudo_live
        ));
        if let Some(reason) = &self.reason {
            out.push_str(&format!("  {:<12}false: {reason}\n", "ready"));
        }
        out
    }

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
            "encode_pending": self.encode_pending,
            "encode_failed": self.encode_failed,
            "encode_oldest_s": self.encode_oldest_s,
            "snapshot_age_s": self.snapshot_age_s,
            "sudo_policies": self.sudo_policies,
            "sudo_live": self.sudo_live,
        })
    }
}

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
        sudo_policies: 0,
        sudo_live: 0,
        ready: false,
        reason: None,
    };
    report.snapshot_age_s = crate::store::snapshot::age_seconds(paths.root());
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

fn read_store(paths: &Paths, report: &mut HealthReport) -> crate::error::Result<bool> {
    index::vec::register_extension();
    let conn = Connection::open_with_flags(paths.db(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    report.schema = db::migrate::version(&conn)?;
    report.memories = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
    report.vectors = match index::vec::stored_dim(&conn)? {
        Some(_) => index::vec::count(&conn)?,
        None => 0,
    };
    let stale = index::vec::projection_stale(&conn)?;
    report.links = conn.query_row("SELECT count(*) FROM memory_link", [], |r| r.get(0))?;
    report.links_dangling = links::dangling(&conn)?.len();
    report.tags = tags::list(&conn)?.len();
    report.projects = project::count(&conn)?;
    report.projects_dangling = project::dangling(&conn)?.len();
    if report.schema >= encode::SCHEMA {
        let queue = encode::counts(&conn)?;
        report.encode_pending = queue.pending;
        report.encode_oldest_s = queue.oldest_s;
    }
    if report.schema >= 24 {
        report.sudo_policies = conn.query_row("SELECT count(*) FROM sudoers", [], |r| r.get(0))?;
        report.sudo_live = conn.query_row(
            "SELECT count(*) FROM active_sudo WHERE expires_at > datetime('now')",
            [],
            |r| r.get(0),
        )?;
    }
    let verdict = encode::projection(&conn)?;
    report.projection = verdict.verdict(report.vectors, report.memories);
    Ok(stale)
}

pub async fn health(
    State(state): State<HttpState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let json = matches!(params.get("format").map(String::as_str), Some("json"));
    let probe_state = state.clone();
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
