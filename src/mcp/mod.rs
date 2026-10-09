//! MCP stdio server (P7) on `rmcp`.
//!
//! Tools: `memory_add`, `memory_update`, `memory_delete`, `memory_search`,
//! `memory_get`, `memory_backlinks`, `memory_export`, `memory_import` — with
//! read-only annotations on the read tools and destructive on `delete`. The tool
//! descriptions carry the SPEC §9 write policy; that is where memory quality is
//! actually tuned.
//!
//! The store is one SQLite file behind a single connection (SPEC §15 #10), so the
//! server holds the [`Session`] behind a mutex and serves requests serially. The
//! `rmcp` runtime is tokio, but every tool body stays synchronous.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

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
    #[schemars(description = "Stable slug. Omit to derive one from scope+title. Re-using a key replaces that memory.")]
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
    #[schemars(description = "Usage class: signpost | dictate | fact | profile. Defaults to fact.")]
    pub type_: Option<String>,
    /// Provenance.
    #[serde(default)]
    #[schemars(description = "Provenance: the command/path/repo that derives this — never live state.")]
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
            id: None,
            key: self.key.unwrap_or_default(),
            scope: self.scope,
            type_: MemoryType::parse(self.type_.as_deref().unwrap_or("fact")),
            title: self.title,
            body: self.body,
            source: self.source,
            hash: String::new(),
            tags: self.tags,
            created: String::new(),
            updated: String::new(),
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

/// One incoming `[[key]]` edge, as returned by `memory_backlinks`.
#[derive(Debug, Serialize)]
struct Backlink {
    key: String,
    scope: String,
    title: String,
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

    /// Run `f` against the store under the lock, turning an [`Error`] into a tool-error string.
    fn with_store<T>(
        &self,
        f: impl FnOnce(&mut Session) -> crate::error::Result<T>,
    ) -> std::result::Result<T, String> {
        let mut guard = self
            .session
            .lock()
            .map_err(|_| "memory store lock poisoned".to_string())?;
        f(&mut guard).map_err(|e| e.to_string())
    }
}

/// One JSON object as the tool's text result.
fn json_ok(value: serde_json::Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// Incoming `[[…]]` edges for `key` (backlinks), ordered by source key.
fn backlinks(conn: &rusqlite::Connection, key: &str) -> crate::error::Result<Vec<Backlink>> {
    let mut stmt = conn.prepare(
        "SELECT m.key, m.scope, m.title FROM memory_link l JOIN memory m ON m.id = l.from_id
          WHERE l.to_key = ?1 ORDER BY m.key",
    )?;
    let rows = stmt.query_map(rusqlite::params![key], |r| {
        Ok(Backlink {
            key: r.get(0)?,
            scope: r.get(1)?,
            title: r.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
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
    fn memory_add(
        &self,
        Parameters(p): Parameters<AddParams>,
    ) -> std::result::Result<String, String> {
        let key = self.with_store(|s| s.add(&p.into_memory()))?;
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
    fn memory_update(
        &self,
        Parameters(p): Parameters<AddParams>,
    ) -> std::result::Result<String, String> {
        let mem = p.into_memory();
        if mem.key.trim().is_empty() {
            return Err("memory_update requires `key` (use memory_add to create)".to_string());
        }
        let key = self.with_store(|s| {
            s.show(&mem.key)?; // must exist: update never silently creates
            s.add(&mem)
        })?;
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
    fn memory_delete(
        &self,
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        if self.with_store(|s| s.rm(&p.key))? {
            Ok(json_ok(
                serde_json::json!({ "status": "removed", "key": p.key }),
            ))
        } else {
            Err(format!("no such memory: {}", p.key))
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
    fn memory_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> std::result::Result<String, String> {
        let mode = match p.mode.as_deref() {
            None => Mode::Hybrid,
            Some(m) => Mode::parse(m).map_err(|e| e.to_string())?,
        };
        let bundle = self.with_store(|s| {
            let q = Query {
                text: p.query.clone(),
                scope: p.scope.clone(),
                k: p.k.unwrap_or_else(|| s.default_k()),
                mode,
                max_distance: p.max_distance,
                expand: p.expand.unwrap_or(0),
            };
            s.search(&q)
        })?;
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
    fn memory_get(
        &self,
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let mem = self.with_store(|s| s.show(&p.key))?;
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
    fn memory_backlinks(
        &self,
        Parameters(p): Parameters<KeyParams>,
    ) -> std::result::Result<String, String> {
        let links = self.with_store(|s| backlinks(s.conn(), &p.key))?;
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
    fn memory_export(
        &self,
        Parameters(p): Parameters<PathParams>,
    ) -> std::result::Result<String, String> {
        let count =
            self.with_store(|s| crate::portable::export::to_jsonl(s.conn(), Path::new(&p.path)))?;
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
    fn memory_import(
        &self,
        Parameters(p): Parameters<ImportParams>,
    ) -> std::result::Result<String, String> {
        let dry_run = p.dry_run.unwrap_or(false);
        let count = self.with_store(|s| {
            crate::portable::import::from_jsonl(s.conn(), Path::new(&p.path), dry_run)
        })?;
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

/// Serve MCP over stdio until the client disconnects (the `llmphant mcp` subcommand).
pub fn serve(paths: Paths, cfg: Config, spec: model::Spec) -> crate::error::Result<()> {
    let server = MemoryServer::new(Session::open(paths, cfg, spec)?);
    // rmcp is async; the rest of llmphant is not, so own the runtime here. `waiting()` keeps
    // the process alive until the client closes the pipe.
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let running = server
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| Error::Config(format!("mcp initialize failed: {e}")))?;
        running
            .waiting()
            .await
            .map_err(|e| Error::Config(format!("mcp server task failed: {e}")))?;
        Ok(())
    })
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
            assert_eq!(ann(name).read_only_hint, Some(true), "{name} must be read-only");
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

