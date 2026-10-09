//! One live store session shared by the one-shot commands and the REPL (PLAN §6 P6).
//!
//! It owns a single writer connection (SPEC §15 #10) and loads the Qwen3 encoder lazily:
//! `--mode fts` and `show`/`rm` never pay for 1.2 GB of weights, while the REPL keeps the
//! model warm across commands (the reason v1 has no daemon — PLAN §2 #4).

use rusqlite::Connection;

use crate::config::{Config, Paths};
use crate::db;
use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::model;
use crate::search::{self, Bundle, Query};
use crate::store::{self, Memory, links, maintain, tags};

/// A store handle plus its (lazily loaded) encoder.
pub struct Session {
    paths: Paths,
    cfg: Config,
    spec: model::Spec,
    conn: Connection,
    embedder: Option<Box<dyn Embedder>>,
}

impl Session {
    /// Open (creating if needed) the store and hold the connection.
    pub fn open(paths: Paths, cfg: Config, spec: model::Spec) -> Result<Self> {
        let conn = db::open(&paths)?;
        Ok(Self {
            paths,
            cfg,
            spec,
            conn,
            embedder: None,
        })
    }

    /// Inject a pre-built encoder instead of loading Qwen3 on first use. The MCP test
    /// harness feeds a deterministic fake, so its round-trip needs no weights (PLAN §7).
    pub fn with_embedder(mut self, embedder: Box<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// The resolved store paths.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// The raw connection (used by `doctor`-style introspection).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Default result count for searches with no explicit `-k` (SPEC §12 config).
    pub fn default_k(&self) -> usize {
        self.cfg.default_k
    }

    /// Default cosine-distance ceiling applied when a search omits `max_distance`
    /// (SPEC §10). `None` means the cap is off.
    pub fn default_max_distance(&self) -> Option<f32> {
        self.cfg.default_max_distance
    }

    /// Load the encoder once; later calls are free. This is the only path that touches
    /// the weights cache, so nothing else in the CLI needs it.
    fn ensure_embedder(&mut self) -> Result<()> {
        if self.embedder.is_none() {
            // Loading ~1.2 GB of weights can take a while; log both ends so a "stuck" first
            // search in `docker logs` is visibly a model load, not a hang.
            tracing::info!(
                model = %self.cfg.model,
                device = %self.cfg.device,
                precision = %self.cfg.precision,
                dim = self.cfg.dim,
                "loading encoder"
            );
            let started = std::time::Instant::now();
            self.embedder = Some(Box::new(super::embed::load_embedder(
                &self.spec, &self.cfg,
            )?));
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "encoder loaded"
            );
        }
        Ok(())
    }

    /// The encoder as a trait object; an error if `ensure_embedder` has not run yet.
    fn embedder(&self) -> Result<&dyn Embedder> {
        self.embedder
            .as_deref()
            .ok_or_else(|| Error::Config("encoder was not loaded".into()))
    }

    /// Upsert a memory (SPEC §7) and return its resolved key.
    pub fn add(&mut self, mem: &Memory) -> Result<String> {
        self.ensure_embedder()?;
        let embedder = self.embedder()?;
        let id = store::add(&self.conn, embedder, mem)?;
        let stored = store::get_by_id(&self.conn, id)?.ok_or(Error::NotFound(mem.key.clone()))?;
        Ok(stored.key)
    }

    /// Fetch one memory by key, or [`Error::NotFound`].
    pub fn show(&self, key: &str) -> Result<Memory> {
        store::get(&self.conn, key)?.ok_or_else(|| Error::NotFound(key.to_string()))
    }

    /// Delete one memory by key; `false` means there was nothing to delete.
    pub fn rm(&self, key: &str) -> Result<bool> {
        store::delete(&self.conn, key)
    }

    /// Run a query, loading the encoder only when there is a phrase to embed.
    pub fn search(&mut self, q: &Query) -> Result<Bundle> {
        let needs_embedder = q
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        if needs_embedder {
            self.ensure_embedder()?;
            return search::run(&self.conn, Some(self.embedder()?), q);
        }
        search::run(&self.conn, None, q)
    }

    /// Every row as an index [`Summary`](store::Summary) — no bodies.
    pub fn summaries(&self) -> Result<Vec<store::Summary>> {
        store::summaries(&self.conn)
    }

    /// Recompute every vector and rebuild the derived vector index, handling a model or
    /// dimension change (P9). Loads the encoder, so it is a deliberately heavy command.
    pub fn reembed(&mut self) -> Result<maintain::Report> {
        self.ensure_embedder()?;
        let embedder = self.embedder()?;
        maintain::reembed(&self.conn, embedder, &self.cfg.instruction)
    }

    /// Rename a key: rewrite `[[old]]` in bodies and repoint links (P9).
    pub fn rename(&self, old: &str, new: &str) -> Result<links::RenameOutcome> {
        links::rename(&self.conn, old, new)
    }

    /// `[[key]]` targets with no matching row, grouped by target (P9 audit).
    pub fn dangling(&self) -> Result<Vec<links::Dangling>> {
        links::dangling(&self.conn)
    }

    /// Every tag with its reference count, alphabetical.
    pub fn tags(&self) -> Result<Vec<(String, i64)>> {
        tags::list(&self.conn)
    }

    /// Every key in the store, alphabetical — the REPL's tab-completion vocabulary.
    pub fn keys(&self) -> Result<Vec<String>> {
        store::keys(&self.conn)
    }
}
