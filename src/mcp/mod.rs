//! MCP server (P7) on `rmcp`: Streamable HTTP (default, the networked transport) or stdio.
//!
//! Tools: `memory_add`, `memory_update`, `memory_delete`, `memory_search`,
//! `memory_get`, `memory_backlinks`, `memory_list` — with read-only annotations on the
//! read tools and destructive on `delete`. The tool descriptions carry the SPEC §9 write
//! policy; that is where memory quality is actually tuned.
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
use crate::search::{Bundle, Query};
use crate::store::Memory;

/// Names of the tools this server exposes.
pub const TOOLS: &[&str] = &[
    "memory_add",
    "memory_update",
    "memory_delete",
    "memory_search",
    "memory_get",
    "memory_backlinks",
    "memory_list",
];

/// Server-level instruction: the read/write policy every client should follow (SPEC §9).
const INSTRUCTIONS: &str = "\
llmphant = memory-rock. Clever caveman share it. \
WRITE: big words in, small words out. Drop filler. Drop articles. Drop glue. One fact, one \
row. Key + tags live inside prose — name sharp. Sharp name, sharp recall. `source` = proof \
stone: command/path/repo that prove fact. Never live state. Live state change; memory not \
change. Reuse `key` — old memory die. Trim, never grow. Cheap to derive? Do not store. \
FIND: hunt, judge, fetch. `memory_search` one phrase, small `k`. Back come keys, titles, \
tags, distance. No bodies. Read titles. `memory_get` only hit you use. None fit? Widen. \
Raise `k`. One small miss is not \"not there\" — never grunt that. Know `key` already? \
`memory_get` straight. No hunt.";

/// `memory_add` description. The write policy lives here because this is what the model reads.
const ADD_DESCRIPTION: &str = "\
Store one memory (upsert by `key`). WRITE smart-caveman: big words in, small words out. \
Drop filler, articles, glue. One fact, one row. `key` = slug = wiki-link target. `tags` = \
aspect of thing (storage, gotchas). `source` = command/path/repo that prove it — never live \
state. Reuse `key` — old memory die. Trim, never grow. `title` = one sharp line. `body` = \
fact. Link memory: `[[other-key]]`.";

/// `memory_update` description.
const UPDATE_DESCRIPTION: &str = "\
Replace memory that already live. `key` must exist — new key? memory_add. Same smart-caveman \
law. Body die whole, born new — trim, no pile on. Links re-hunt from `[[…]]` in new body.";

/// Parameters for [`MemoryServer::memory_add`] / [`MemoryServer::memory_update`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddParams {
    /// Stable slug; omit to derive one from the title.
    #[serde(default)]
    #[schemars(
        description = "Stable slug. Omit it, one grow from title. Reuse key, old memory die."
    )]
    pub key: Option<String>,
    /// Salient one-liner.
    #[schemars(description = "One sharp line. Big words, no filler.")]
    pub title: String,
    /// The fact.
    #[schemars(description = "Fact. Drop filler. Link memory with `[[other-key]]`.")]
    pub body: String,
    /// Provenance.
    #[serde(default)]
    #[schemars(description = "Proof stone: command/path/repo that derive this. Never live state.")]
    pub source: Option<String>,
    /// Aspect tags.
    #[serde(default)]
    #[schemars(description = "Aspects of the subject, e.g. storage, gotchas.")]
    pub tags: Vec<String>,
}

impl AddParams {
    /// Convert the request into the storage/embedding unit.
    fn into_memory(self) -> Memory {
        Memory {
            key: self.key.unwrap_or_default(),
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
    /// The phrase to match.
    #[serde(default)]
    #[schemars(description = "Phrase to hunt with. Matched against embedded document.")]
    pub query: Option<String>,
    /// Result count.
    #[serde(default)]
    #[schemars(description = "Most hits bring back (default = configured k).")]
    pub k: Option<usize>,
    /// Cosine-distance ceiling.
    #[serde(default)]
    #[schemars(description = "Cosine-distance ceiling. Smaller = closer.")]
    pub max_distance: Option<f32>,
    /// Link-neighbourhood hops.
    #[serde(default)]
    #[schemars(description = "Link-neighbour hops drag in (0 = off).")]
    pub expand: Option<u8>,
}

/// A single-key request.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KeyParams {
    /// The memory's key.
    #[schemars(description = "Memory key. Slug.")]
    pub key: String,
}

/// Parameters for [`MemoryServer::memory_list`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListParams {}

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
    /// this must not occupy a tokio worker.
    ///
    /// `tool` labels the call in logs: a success is a `debug!` with its latency, a store
    /// error a `warn!`, and a panic/join failure an `error!`. That is the one place every
    /// tool failure surfaces, so `docker logs` never loses it to a client-side error string.
    async fn with_store<T, F>(&self, tool: &'static str, f: F) -> std::result::Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(&mut Session) -> crate::error::Result<T> + Send + 'static,
    {
        let session = Arc::clone(&self.session);
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
}

/// One JSON object as the tool's text result.
fn json_ok(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// Log one `memory_search` round trip: the request, every returned hit (key/tags/distance) and
/// the size of the serialized response. Visible with `RUST_LOG=llmphant=info`; the Docker image
/// sets that by default.
#[allow(clippy::too_many_arguments)] // one structured log line; splitting it would hurt grepping
fn log_search(
    query: Option<&str>,
    expand: u8,
    bundle: &Bundle,
    response_bytes: usize,
    elapsed_ms: u64,
) {
    tracing::info!(
        query = query.unwrap_or("-"),
        expand,
        hits = bundle.hits.len(),
        neighbors = bundle.neighbors.len(),
        response_bytes,
        elapsed_ms,
        "memory_search"
    );
    for hit in &bundle.hits {
        tracing::info!(
            key = %hit.key,
            tags = ?hit.tags,
            distance = ?hit.distance,
            "  hit"
        );
    }
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
        let mem = p.into_memory();
        let tags = mem.tags.clone();
        let body_bytes = mem.body.len();
        let key = self.with_store("memory_add", move |s| s.add(&mem)).await?;
        tracing::info!(tool = "memory_add", key = %key, tags = ?tags, body_bytes, "memory stored");
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
        let tags = mem.tags.clone();
        let body_bytes = mem.body.len();
        let key = self
            .with_store("memory_update", move |s| {
                s.show(&mem.key)?; // must exist: update never silently creates
                s.add(&mem)
            })
            .await?;
        tracing::info!(tool = "memory_update", key = %key, tags = ?tags, body_bytes, "memory updated");
        Ok(json_ok(
            serde_json::json!({ "status": "updated", "key": key }),
        ))
    }

    /// Delete one memory by key.
    #[tool(
        name = "memory_delete",
        description = "Kill one memory by `key`. No undo. Gone.",
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
        if self
            .with_store("memory_delete", move |s| s.rm(&rm_key))
            .await?
        {
            tracing::info!(tool = "memory_delete", key = %key, "memory removed");
            Ok(json_ok(
                serde_json::json!({ "status": "removed", "key": key }),
            ))
        } else {
            tracing::warn!(tool = "memory_delete", key = %key, "no such memory");
            Err(format!("no such memory: {key}"))
        }
    }

    /// Semantic phrase search: KNN over the document embeddings.
    #[tool(
        name = "memory_search",
        description = "Hunt memory by meaning. Whole document (key, tags, title, body) sit in embedding. Phrase touch any part, memory rise. Back come likeliest keys: {title, tags, distance}. No bodies. Start small `k`. Read titles. `memory_get` only hit you use. None fit? Widen. Raise `k`. Empty? Loosen `max_distance`. Know `key`? memory_get instead.",
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
        let query = p.query;
        let k = p.k;
        let max_distance = p.max_distance;
        let expand = p.expand.unwrap_or(0);
        // Keep a copy for the log line: the original moves into the store closure.
        let log_query = query.clone();
        let started = std::time::Instant::now();
        let bundle = self
            .with_store("memory_search", move |s| {
                let q = Query {
                    text: query,
                    k: k.unwrap_or_else(|| s.default_k()),
                    // Fall back to the configured ceiling so a bare semantic search is capped.
                    max_distance: max_distance.or_else(|| s.default_max_distance()),
                    expand,
                };
                s.search(&q)
            })
            .await?;
        let json = serde_json::to_string(&bundle).map_err(|e| e.to_string())?;
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
        description = "Pull one memory whole by `key`. Only for hit you use.",
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
        let key = p.key;
        let lookup = key.clone();
        let mem = self
            .with_store("memory_get", move |s| s.show(&lookup))
            .await?;
        tracing::debug!(tool = "memory_get", key = %key, "memory fetched");
        serde_json::to_string(&mem).map_err(|e| e.to_string())
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
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let key = p.key;
        let lookup = key.clone();
        let links = self
            .with_store("memory_backlinks", move |s| s.backlinks(&lookup))
            .await?;
        tracing::debug!(tool = "memory_backlinks", key = %key, count = links.len(), "backlinks");
        serde_json::to_string(&links).map_err(|e| e.to_string())
    }

    /// List the store's index (no bodies).
    #[tool(
        name = "memory_list",
        description = "List whole index: key, tags, title every memory. No bodies. Browse. Pick keys before get/search.",
        annotations(
            title = "List memories",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn memory_list(
        &self,
        Parameters(_): Parameters<ListParams>,
    ) -> std::result::Result<String, String> {
        let memories = self
            .with_store("memory_list", move |s| s.summaries())
            .await?;
        tracing::info!(tool = "memory_list", count = memories.len(), "listed");
        Ok(json_ok(serde_json::json!({
            "count": memories.len(),
            "memories": memories,
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
    tracing::info!(
        version = crate::VERSION,
        db = %paths.db().display(),
        model = %cfg.model,
        dim = cfg.dim,
        device = %cfg.device,
        precision = %cfg.precision,
        offline = spec.offline,
        "starting llmphant mcp"
    );
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
            "memory_list",
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
        assert!(add.to_lowercase().contains("one fact"), "{add}");
        assert!(add.contains("source"), "{add}");

        // Search filters by phrase only — no key/tag constraints anywhere in the schema.
        let props = find("memory_search")
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .expect("search schema has properties");
        for gone in ["key", "tags", "tag_match", "field", "match"] {
            assert!(
                !props.contains_key(gone),
                "memory_search must not expose {gone}: {props:?}"
            );
        }
        // Only the phrase and its ranking knobs remain.
        let mut searchable: Vec<&str> = props.keys().map(String::as_str).collect();
        searchable.sort();
        assert_eq!(searchable, ["expand", "k", "max_distance", "query"]);
        // `memory_list` is unfiltered now (no `tag`).
        let list_props = find("memory_list")
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        assert!(!list_props.contains_key("tag"), "{list_props:?}");

        // The read policy tells the model to search narrow and widen.
        let info = MemoryServer::new(memory_session()).get_info();
        let instructions = info.instructions.expect("instructions").to_lowercase();
        assert!(instructions.contains("widen"), "{instructions}");
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
