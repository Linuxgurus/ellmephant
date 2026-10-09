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
use crate::store::{self, Memory};

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

    /// Load the encoder once; later calls are free. This is the only path that touches
    /// the weights cache, so nothing else in the CLI needs it.
    fn ensure_embedder(&mut self) -> Result<()> {
        if self.embedder.is_none() {
            self.embedder = Some(Box::new(super::load_embedder(&self.spec, &self.cfg)?));
        }
        Ok(())
    }

    /// The encoder as a trait object, or `None` while it is still cold.
    fn embedder(&self) -> Option<&dyn Embedder> {
        self.embedder.as_deref()
    }

    /// Upsert a memory (SPEC §7) and return its resolved key.
    pub fn add(&mut self, mem: &Memory) -> Result<String> {
        self.ensure_embedder()?;
        let embedder = self.embedder.as_deref().expect("encoder was just loaded");
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

    /// Run a query, loading the encoder only when the mode's KNN leg needs it.
    pub fn search(&mut self, q: &Query) -> Result<Bundle> {
        if q.mode.needs_embedder() {
            self.ensure_embedder()?;
        }
        search::run(&self.conn, self.embedder(), q)
    }

    /// Every tag with its reference count, alphabetical.
    pub fn tags(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.name, count(mt.memory_id)
               FROM tag t LEFT JOIN memory_tag mt ON mt.tag_id = t.id
              GROUP BY t.id ORDER BY t.name",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every key in the store, alphabetical — the REPL's tab-completion vocabulary.
    pub fn keys(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT key FROM memory ORDER BY key")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
