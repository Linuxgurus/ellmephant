//! MCP server (P7) on `rmcp`: Streamable HTTP (default, the networked transport) or stdio.
//!
//! Tools: `memory_add`, `memory_update`, `memory_delete`, `memory_search`,
//! `memory_get`, `memory_backlinks`, `memory_export`, `memory_import` — with
//! read-only annotations on the read tools and destructive on `delete`. The tool
//! descriptions carry the SPEC §9 write policy; that is where memory quality is
//! actually tuned.
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

use std::path::Path;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::cli::session::Session;
use crate::config::{Config, Paths};
// NOTE: do *not* import `crate::error::Result` here — `#[tool_handler]` expands to code that
// uses the unqualified `Result` from `rmcp`, and our single-parameter alias would shadow it.
use crate::error::Error;
use crate::model;
use crate::search::{Mode, Query};
use crate::store::{Memory, MemoryType};

/// Names of the tools this server exposes.
pub const TOOLS: &[&str] = &[
    "memory_add",
    "memory_update",
    "memory_delete",
    "memory_search",
    "memory_get",
    "memory_backlinks",
    "memory_export",
    "memory_import",
];

/// Server-level instruction: the read/write policy every client should follow (SPEC §9).
const INSTRUCTIONS: &str = "\
llmphant is a shared, atomic long-term memory. Write one fact per row: type it \
(signpost|dictate|fact|profile), put provenance in `source` (the command/path/repo that \
derives the fact, never live state), and upsert by `key` — trim on edit, never grow, and \
never store cheaply-derivable state. Search returns scored snippets: read them, don't dump \
them.";

/// `memory_add` description. The write policy lives here because this is what the model reads.
const ADD_DESCRIPTION: &str = "\
Store one atomic memory (upsert by `key`). Write policy (SPEC §9): one fact per row; `type` \
is signpost|dictate|fact|profile; `source` holds provenance (the command/path/repo that \
derives the fact), never live state; re-using a `key` replaces that memory (trim on edit, \
never grow); never store cheaply-derivable state. `title` is a salient one-liner, `body` the \
full verifiable fact; wiki links are `[[other-key]]`.";

/// `memory_update` description.
const UPDATE_DESCRIPTION: &str = "\
Replace an existing memory; `key` must already exist (use memory_add to create). Same write \
policy as memory_add: the body is replaced wholesale, so trim rather than append, and links \
are re-derived from `[[…]]` in the new body.";

/// Parameters for [`MemoryServer::memory_add`] / [`MemoryServer::memory_update`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddParams {
    /// Stable slug; omit to derive one from `scope` + `title`.
    #[serde(default)]
    #[schemars(
        description = "Stable slug. Omit to derive one from scope+title. Re-using a key replaces that memory."
    )]
    pub key: Option<String>,
    /// Single bucket, e.g. `infra`.
    #[schemars(description = "Single bucket, e.g. `infra` or `projects`.")]
    pub scope: String,
    /// Salient one-liner.
    #[schemars(description = "A salient one-liner.")]
    pub title: String,
    /// Full, verifiable body.
    #[schemars(description = "The full, verifiable body. Wiki links are written `[[other-key]]`.")]
    pub body: String,
    /// Usage class; default `fact`.
    #[serde(rename = "type", default)]
    #[schemars(
        description = "Usage class: signpost | dictate | fact | profile. Defaults to fact."
    )]
    pub type_: Option<String>,
    /// Provenance.
    #[serde(default)]
    #[schemars(
        description = "Provenance: the command/path/repo that derives this — never live state."
    )]
    pub source: Option<String>,
    /// Free-form tags.
    #[serde(default)]
    #[schemars(description = "Free-form tags.")]
    pub tags: Vec<String>,
}

impl AddParams {
    /// Convert the request into the storage/embedding unit.
    fn into_memory(self) -> Memory {
        Memory {
            key: self.key.unwrap_or_default(),
            scope: self.scope,
            type_: MemoryType::parse(self.type_.as_deref().unwrap_or("fact")),
            title: self.title,
            body: self.body,
            source: self.source,
            tags: self.tags,
            ..Default::default()
        }
    }
}

/// Parameters for [`MemoryServer::memory_search`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// The question.
    #[schemars(description = "The question to answer from memory.")]
    pub query: String,
    /// Restrict to one bucket.
    #[serde(default)]
    #[schemars(description = "Restrict to one bucket.")]
    pub scope: Option<String>,
    /// Result count.
    #[serde(default)]
    #[schemars(description = "Maximum number of hits (defaults to the configured k).")]
    pub k: Option<usize>,
    /// Retrieval mode.
    #[serde(rename = "mode", default)]
    #[schemars(description = "Retrieval mode: hybrid (default) | vec | fts.")]
    pub mode: Option<String>,
    /// Cosine-distance ceiling.
    #[serde(default)]
    #[schemars(description = "Optional cosine-distance ceiling for the vector leg.")]
    pub max_distance: Option<f32>,
    /// Link-neighbourhood hops.
    #[serde(default)]
    #[schemars(description = "Link-neighbourhood hops to include (0 = off).")]
    pub expand: Option<u8>,
}

/// A single-key request.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeyParams {
    /// The memory's key.
    #[schemars(description = "The memory's key (slug).")]
    pub key: String,
}

/// A filesystem-path request (export).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PathParams {
    /// Destination path.
    #[schemars(description = "Filesystem path, on the machine running llmphant.")]
    pub path: String,
}

/// A filesystem-path request (import).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImportParams {
    /// Source path.
    #[schemars(description = "JSONL path, on the machine running llmphant.")]
    pub path: String,
    /// Report the diff without writing.
    #[serde(default)]
    #[schemars(description = "Report what would change without writing.")]
    pub dry_run: Option<bool>,
}

/// The MCP server. Holds the single store [`Session`] behind a mutex; `#[tool_handler]`
/// reaches the tool table through the generated `Self::tool_router()`, so there is no field.
#[derive(Clone)]
pub struct MemoryServer {
    session: Arc<Mutex<Session>>,
}

impl MemoryServer {
    /// Build a server over an open session — the `llmphant mcp` path.
    pub fn new(session: Session) -> Self {
        Self::from_shared(Arc::new(Mutex::new(session)))
    }

    /// Build a server over a session already behind a shared lock (tests).
    pub fn from_shared(session: Arc<Mutex<Session>>) -> Self {
        Self { session }
    }

    /// Run `f` against the store on a blocking thread, turning an [`Error`] into a
    /// tool-error string. SQLite and the encoder can hold the store lock for a while, so
    /// this must not occupy a tokio worker (REFACTOR follow-up).
    async fn with_store<T, F>(&self, f: F) -> std::result::Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session) -> crate::error::Result<T> + Send + 'static,
    {
        let session = Arc::clone(&self.session);
        tokio::task::spawn_blocking(move || {
            let mut guard = session
                .lock()
                .map_err(|_| "memory store lock poisoned".to_string())?;
            f(&mut guard).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| format!("memory store task failed: {e}"))?
    }
}

/// One JSON object as the tool's text result.
fn json_ok(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

#[tool_router]
impl MemoryServer {
    /// Store one atomic memory (upsert by key); see the description for the write policy.
    #[tool(
        name = "memory_add",
        description = ADD_DESCRIPTION,
        annotations(
            title = "Add memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_add(
        &self,
        Parameters(p): Parameters<AddParams>,
    ) -> std::result::Result<String, String> {
        let key = self.with_store(move |s| s.add(&p.into_memory())).await?;
        Ok(json_ok(
            serde_json::json!({ "status": "stored", "key": key }),
        ))
    }

    /// Replace an existing memory; fails when the key is unknown.
    #[tool(
        name = "memory_update",
        description = UPDATE_DESCRIPTION,
        annotations(
            title = "Update memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_update(
        &self,
        Parameters(p): Parameters<AddParams>,
    ) -> std::result::Result<String, String> {
        let mem = p.into_memory();
        if mem.key.trim().is_empty() {
            return Err("memory_update requires `key` (use memory_add to create)".to_string());
        }
        let key = self
            .with_store(move |s| {
                s.show(&mem.key)?; // must exist: update never silently creates
                s.add(&mem)
            })
            .await?;
        Ok(json_ok(
            serde_json::json!({ "status": "updated", "key": key }),
        ))
    }

    /// Delete one memory by key.
    #[tool(
        name = "memory_delete",
        description = "Delete one memory by `key`. Irreversible.",
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
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let key = p.key;
        let rm_key = key.clone();
        if self.with_store(move |s| s.rm(&rm_key)).await? {
            Ok(json_ok(
                serde_json::json!({ "status": "removed", "key": key }),
            ))
        } else {
            Err(format!("no such memory: {key}"))
        }
    }

    /// Hybrid search; returns scored snippets plus (optionally) their link neighbourhood.
    #[tool(
        name = "memory_search",
        description = "Hybrid (BM25 ∪ KNN, RRF-fused) search over memory. Returns scored snippets — read them, don't dump them. `mode=fts` needs no model.",
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
        Parameters(p): Parameters<SearchParams>,
    ) -> std::result::Result<String, String> {
        let mode = match p.mode.as_deref() {
            None => Mode::Hybrid,
            Some(m) => Mode::parse(m).map_err(|e| e.to_string())?,
        };
        let query = p.query;
        let scope = p.scope;
        let k = p.k;
        let max_distance = p.max_distance;
        let expand = p.expand.unwrap_or(0);
        let bundle = self
            .with_store(move |s| {
                let q = Query {
                    text: query,
                    scope,
                    k: k.unwrap_or_else(|| s.default_k()),
                    mode,
                    max_distance,
                    expand,
                    rrf_k: crate::search::DEFAULT_RRF_K,
                };
                s.search(&q)
            })
            .await?;
        serde_json::to_string(&bundle).map_err(|e| e.to_string())
    }

    /// Fetch one memory in full by key.
    #[tool(
        name = "memory_get",
        description = "Fetch one memory in full by `key`.",
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
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let mem = self.with_store(move |s| s.show(&p.key)).await?;
        serde_json::to_string(&mem).map_err(|e| e.to_string())
    }

    /// List the memories whose bodies link to `key`.
    #[tool(
        name = "memory_backlinks",
        description = "List the memories whose bodies link to `key` via `[[…]]`.",
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
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let links = self
            .with_store(move |s| crate::store::links::backlinks(s.conn(), &p.key))
            .await?;
        serde_json::to_string(&links).map_err(|e| e.to_string())
    }

    /// Export the whole store to JSONL.
    #[tool(
        name = "memory_export",
        description = "Export the whole store to JSONL at `path` (canonical fields; no ids or vectors).",
        annotations(
            title = "Export memory",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_export(
        &self,
        Parameters(p): Parameters<PathParams>,
    ) -> std::result::Result<String, String> {
        let path = p.path.clone();
        let count = self.with_store(move |s| s.export(Path::new(&path))).await?;
        Ok(json_ok(
            serde_json::json!({ "status": "exported", "path": p.path, "count": count }),
        ))
    }

    /// Import JSONL into the store (upsert by key).
    #[tool(
        name = "memory_import",
        description = "Import JSONL from `path`, upserting by key. `dry_run` reports the diff without writing.",
        annotations(
            title = "Import memory",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_import(
        &self,
        Parameters(p): Parameters<ImportParams>,
    ) -> std::result::Result<String, String> {
        let dry_run = p.dry_run.unwrap_or(false);
        let path = p.path.clone();
        let count = self
            .with_store(move |s| s.import(Path::new(&path), dry_run))
            .await?;
        Ok(json_ok(serde_json::json!({
            "status": if dry_run { "dry-run" } else { "imported" },
            "path": p.path,
            "count": count,
        })))
    }
}

#[tool_handler]
impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("llmphant", crate::VERSION))
            .with_instructions(INSTRUCTIONS)
    }
}

/// How `llmphant mcp` reaches its client.
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout (a client launches the process).
    Stdio,
    /// MCP Streamable HTTP — the default networked transport. The tool surface is mounted at
    /// `/mcp` on `host:port`.
    Http { host: String, port: u16 },
}

/// Serve MCP on the chosen [`Transport`] until it is stopped (the `llmphant mcp` subcommand).
/// HTTP is the default; stdio stays available for client-launched setups.
pub fn serve(
    paths: Paths,
    cfg: Config,
    spec: model::Spec,
    transport: Transport,
) -> crate::error::Result<()> {
    let server = MemoryServer::new(Session::open(paths, cfg, spec)?);
    // rmcp is async; the rest of llmphant is not, so own the runtime here.
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        match transport {
            Transport::Stdio => {
                // `waiting()` keeps the process alive until the client closes the pipe.
                let running = server
                    .serve(rmcp::transport::stdio())
                    .await
                    .map_err(|e| Error::Config(format!("mcp initialize failed: {e}")))?;
                running
                    .waiting()
                    .await
                    .map_err(|e| Error::Config(format!("mcp server task failed: {e}")))?;
                Ok(())
            }
            Transport::Http { host, port } => serve_http(server, &host, port).await,
        }
    })
}

/// Mount the tool surface at `/mcp` as an `axum` router. Each MCP session gets its own
/// `MemoryServer` clone, all sharing the one [`Session`] behind the mutex (SPEC §15 #10).
///
/// `bind_host` decides the DNS-rebinding guard: on a loopback bind the built-in allow-list
/// (localhost / 127.0.0.1 / ::1) stands; any other bind (e.g. `0.0.0.0` in Docker) disables
/// it, since remote clients arrive with a Host header we cannot enumerate. There is no auth —
/// bind wide only behind a trusted network.
pub fn http_router(server: MemoryServer, bind_host: &str) -> axum::Router {
    let config = if is_loopback(bind_host) {
        StreamableHttpServerConfig::default()
    } else {
        tracing::warn!(
            host = bind_host,
            "mcp http bound off-loopback; host-header checks off"
        );
        StreamableHttpServerConfig::default().disable_allowed_hosts()
    };
    let service: StreamableHttpService<MemoryServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            LocalSessionManager::default().into(),
            config,
        );
    axum::Router::new().nest_service("/mcp", service)
}

/// True for `localhost`, `127.0.0.0/8` and `::1`.
fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// Bind `host:port`, serve HTTP until Ctrl-C, and drain in-flight requests (SPEC §13).
async fn serve_http(server: MemoryServer, host: &str, port: u16) -> crate::error::Result<()> {
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "llmphant mcp listening on http://{addr}/mcp");
    let app = http_router(server, host);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolves when the process is asked to stop, so `axum` can finish open requests.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("mcp shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "memory_export",
        ] {
            assert_eq!(
                ann(name).read_only_hint,
                Some(true),
                "{name} must be read-only"
            );
        }
        // Delete is the destructive one; add/update are idempotent upserts.
        assert_eq!(ann("memory_delete").destructive_hint, Some(true));
        assert_eq!(ann("memory_delete").read_only_hint, Some(false));
        assert_eq!(ann("memory_add").idempotent_hint, Some(true));
        assert_eq!(ann("memory_update").idempotent_hint, Some(true));

        // The write policy rides on the descriptions (SPEC §9).
        let add = find("memory_add").description.clone().expect("description");
        assert!(add.contains("one fact per row"), "{add}");
        assert!(add.contains("source"), "{add}");
    }
}
