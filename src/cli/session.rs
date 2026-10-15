//! One live store session: the store handle the server and the CLI share (PLAN §6 P6).
//!
//! It owns a single writer connection (SPEC §15 #10) and loads the Qwen3 encoder lazily, so a
//! command that never embeds (say `doctor`) never pays for 1.2 GB of weights while a running server
//! keeps the model warm across calls (the reason v1 has no daemon — PLAN §2 #4).
//!
//! The surface is deliberately kept to what `mcp` and `doctor` actually reach for. A wrapper with
//! no caller reads like a load-bearing one, and this file collected several: `paths`, `rename`,
//! `dangling`, `tags` and `keys` all sat here, unused, for as long as they did.

use rusqlite::Connection;

use crate::config::{Config, Paths};
use crate::db;
use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::model;
use crate::search::{self, Bundle, Query};
use crate::store::{self, Memory, links, maintain, project};

/// A store handle plus its (lazily loaded) encoder.
pub struct Session {
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

    /// Upsert a memory from a partial patch (SPEC §7): `None` fields keep their stored value, so one
    /// field can change without resending the row. The encoder is always loaded — the document may
    /// have moved — but the store re-embeds only when it actually did.
    pub fn memory_merge(&mut self, patch: &store::Patch) -> Result<store::MergeOutcome> {
        self.ensure_embedder()?;
        let embedder = self.embedder()?;
        store::merge(&self.conn, embedder, patch)
    }

    /// Fetch one memory by key without counting a visit. Private on purpose: `fetch` is the read
    /// path, and "read without counting" is not something a tool should be able to reach.
    fn show(&self, key: &str) -> Result<Memory> {
        store::get(&self.conn, key)?.ok_or_else(|| Error::NotFound(key.to_string()))
    }

    /// Retire one memory into another: repoint every reference, then delete the source. The prose
    /// fold is the caller's — this never touches `into`'s body. No encoder: the graph is not
    /// embedded. (Distinct from [`memory_merge`](Self::memory_merge), which is the *field* patch
    /// behind `memory_upsert`.)
    pub fn merge_into(&self, from: &str, into: &str) -> Result<links::MergeInto> {
        links::merge_into(&self.conn, from, into)
    }

    /// Fetch one memory and count the read (`visits`, SPEC §9 telemetry) — the read path's entry
    /// point, backing `memory_get`.
    pub fn fetch(&self, key: &str) -> Result<Memory> {
        let mut mem = self.show(key)?;
        mem.visits = store::record_visit(&self.conn, key)?;
        Ok(mem)
    }

    /// Bump `times_useful` for many keys at once. Returns `(key, new_total)` for the keys that
    /// existed and the keys that did not, both in input order — a missing key is not an error.
    pub fn record_useful_many(&self, keys: &[String]) -> Result<store::UsefulSweep> {
        store::record_useful_many(&self.conn, keys)
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

    /// A read-only digest of the store (`stats`); `db_bytes` comes from the store file, so the
    /// MCP surface reports the real size rather than a zero the caller was expected to fill.
    /// `with_tags` additionally fills the full tag vocabulary — the digest stays lean without it.
    pub fn stats(&self, with_tags: bool) -> Result<store::stats::Stats> {
        let mut stats = store::stats::compute(&self.conn)?;
        if with_tags {
            stats.all_tags = store::stats::vocabulary(&self.conn)?;
        }
        Ok(stats)
    }

    /// Recompute every vector and rebuild the derived indexes — the vector index and the `[[link]]`
    /// graph — handling a model or dimension change (P9). Loads the encoder, so it is the heaviest
    /// call the server makes; it backs the `memory_reembed` tool.
    pub fn reembed(&mut self) -> Result<maintain::Report> {
        self.ensure_embedder()?;
        let embedder = self.embedder()?;
        maintain::reembed(&self.conn, embedder, &self.cfg.instruction)
    }

    /// Every row whose body links to `key` — the MCP `memory_backlinks` surface.
    pub fn backlinks(&self, key: &str) -> Result<Vec<links::Backlink>> {
        links::backlinks(&self.conn, key)
    }

    /// Upsert a project from a partial patch: `None` fields keep their stored value. No encoder — a
    /// project is structure, not a document.
    pub fn project_merge(&self, patch: &project::Patch) -> Result<project::MergeOutcome> {
        project::merge(&self.conn, patch)
    }

    /// One project with both relations resolved — the "stand inside it" read. `hydrate` decides how
    /// much of each preload memory's body to carry.
    pub fn project_view(&self, key: &str, hydrate: project::Hydrate) -> Result<project::View> {
        project::view(&self.conn, key, hydrate)?
            .ok_or_else(|| Error::NoProject(project::normalize_key(key)))
    }

    /// Projects matching `filter`, key order.
    pub fn project_list(&self, filter: &project::Filter) -> Result<Vec<project::Project>> {
        project::list(&self.conn, filter)
    }
}
