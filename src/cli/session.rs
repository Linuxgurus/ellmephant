//! One live store session shared by the one-shot commands and the REPL (PLAN §6 P6).
//!
//! It owns a single writer connection (SPEC §15 #10) and loads the Qwen3 encoder lazily:
//! `show`/`rm` and the link/tag maintenance commands never pay for 1.2 GB of weights, while
//! the REPL keeps the model warm across commands (the reason v1 has no daemon — PLAN §2 #4).

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
        let (_id, key) = store::add(&self.conn, embedder, mem)?;
        Ok(key)
    }

    /// Fetch one memory by key, or [`Error::NotFound`].
    pub fn show(&self, key: &str) -> Result<Memory> {
        store::get(&self.conn, key)?.ok_or_else(|| Error::NotFound(key.to_string()))
    }

    /// Fetch one memory and count the read (the `visits` telemetry). The read path uses this;
    /// the write path's existence check uses [`show`](Self::show), which must not count.
    pub fn fetch(&self, key: &str) -> Result<Memory> {
        let mut mem = self.show(key)?;
        mem.visits = store::record_visit(&self.conn, key)?;
        Ok(mem)
    }

    /// Bump the `times_useful` counter for `key` (explicit agent feedback); returns the new total.
    pub fn record_useful(&self, key: &str) -> Result<i64> {
        store::record_useful(&self.conn, key)
    }

    /// Set the split-review verdict for `key` (`Some(true)` coherent, `Some(false)` a
    /// grab-bag, `None` unjudged). Errors with [`Error::NotFound`] when the key is unknown.
    pub fn mark_coherent(&self, key: &str, coherent: Option<bool>) -> Result<()> {
        if store::set_coherent(&self.conn, key, coherent)? {
            Ok(())
        } else {
            Err(Error::NotFound(key.to_string()))
        }
    }

    /// Find near-duplicate candidate pairs (the merge finder, SPEC §9). No encoder needed —
    /// it reads the stored document vectors.
    pub fn dupes(&self, max_distance: f32, limit: usize) -> Result<Vec<store::dupes::DupePair>> {
        store::dupes::find(&self.conn, max_distance, limit)
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

    /// A read-only digest of the store (`stats`); `db_bytes` is filled from the store file.
    pub fn stats(&self) -> Result<store::stats::Stats> {
        store::stats::compute(&self.conn)
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

    /// Every row whose body links to `key` — the MCP `memory_backlinks` surface.
    pub fn backlinks(&self, key: &str) -> Result<Vec<links::Backlink>> {
        links::backlinks(&self.conn, key)
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
