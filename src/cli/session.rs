//! One live store session: the store handle the server and the CLI share (PLAN §6 P6).
//!
//! It owns a single writer connection (SPEC §15 #10) and loads the Qwen3 encoder lazily, so a
//! command that never embeds (say `doctor`) never pays for 1.2 GB of weights while a running server
//! keeps the model warm across calls (the reason v1 has no daemon — PLAN §2 #4).
//!
//! The surface is deliberately kept to what `mcp` and `doctor` actually reach for. A wrapper with
//! no caller reads like a load-bearing one, and this file collected several: `paths`, `rename`,
//! `dangling`, `tags` and `keys` all sat here, unused, for as long as they did.

use std::sync::Arc;

use rusqlite::Connection;

use crate::config::{Config, Paths};
use crate::db;
use crate::embed::{Embedder, VecSpec};
use crate::error::{Error, Result};
use crate::model;
use crate::search::{self, Bundle, Query};
use crate::store::{self, Memory, claim, encode, links, maintain, project};

/// The store's encoder: lazily loaded, and shared by the writer and by every reader.
///
/// **Lock-free after the load, and that is the fix, not a detail.** Embedding is the slow part of a
/// write (~1.3-8.5 s per KB of body), and it used to run while the caller held the store lock — which
/// is what turned one write into a store-wide stall. Giving the encoder its own lock only *moved* the
/// stall: a semantic search then waited out the write's entire embed instead of the store's. So there
/// is no lock over the weights at all. `Embedder` is `Send + Sync` for exactly this: two inferences at
/// once contend for CPU and both finish, where serializing them makes the reader wait for the writer.
///
/// Loading happens once, on first use: ~1.2 GB of weights that a command which never embeds never
/// pays for, while a running server keeps them warm across calls.
pub struct Encoder {
    spec: model::Spec,
    cfg: Config,
    /// The weights. A `OnceLock`, deliberately not a mutex — see the note above.
    inner: std::sync::OnceLock<Box<dyn Embedder>>,
    /// Held only while the weights LOAD — once, ever — never while they are used.
    loading: std::sync::Mutex<()>,
}

impl Encoder {
    /// A shared handle over the weights, not yet loaded.
    pub fn new(spec: model::Spec, cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            spec,
            cfg,
            inner: std::sync::OnceLock::new(),
            loading: std::sync::Mutex::new(()),
        })
    }

    /// Run `f` with the encoder, loading it on first use.
    pub fn with<T>(&self, f: impl FnOnce(&dyn Embedder) -> Result<T>) -> Result<T> {
        if let Some(embedder) = self.inner.get() {
            return f(embedder.as_ref());
        }
        // Only the first use arrives here, and two threads may arrive together: this lock serializes
        // the LOAD. It is released before `f`, so the first caller's work cannot block a second
        // caller's load.
        {
            let _loading = self.loading.lock().unwrap_or_else(|e| e.into_inner());
            if self.inner.get().is_none() {
                // Log both ends: a "stuck" first search in `docker logs` must be visibly a model
                // load, not a hang.
                tracing::info!(
                    model = %self.cfg.model,
                    device = %self.cfg.device,
                    precision = %self.cfg.precision,
                    dim = self.cfg.dim,
                    "loading encoder"
                );
                let started = std::time::Instant::now();
                let loaded: Box<dyn Embedder> =
                    Box::new(super::embed::load_embedder(&self.spec, &self.cfg)?);
                // Losing a race is fine: whoever set it first loaded the same weights.
                let _ = self.inner.set(loaded);
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "encoder loaded"
                );
            }
        }
        f(self.inner.get().expect("set just above").as_ref())
    }

    /// Inject a pre-built encoder instead of loading Qwen3 — the MCP and HTTP harnesses feed a
    /// deterministic fake so their round trips need no weights (PLAN §7). Must run before first use.
    pub fn inject(&self, embedder: Box<dyn Embedder>) {
        let _ = self.inner.set(embedder);
    }
}

/// A store handle: one connection, the settings, and a share of the encoder.
pub struct Session {
    paths: Paths,
    cfg: Config,
    conn: Connection,
    encoder: Arc<Encoder>,
}

impl Session {
    /// Open (creating if needed) the store and hold the connection.
    pub fn open(paths: Paths, cfg: Config, spec: model::Spec) -> Result<Self> {
        let conn = db::open(&paths)?;
        let encoder = Encoder::new(spec, cfg.clone());
        Ok(Self {
            paths,
            cfg,
            conn,
            encoder,
        })
    }

    /// A READ-ONLY session: its own connection, the shared encoder, and no store lock. This is how a
    /// read is served while a write is in flight — the writer holds its session for the whole write,
    /// embedding included, so a read that queued on it would wait out the write.
    pub fn read_only(paths: &Paths, cfg: Config, encoder: Arc<Encoder>) -> Result<Self> {
        Ok(Self {
            paths: paths.clone(),
            cfg,
            conn: db::open_read_only(paths)?,
            encoder,
        })
    }

    /// Inject a pre-built encoder (see [`Encoder::inject`]) — the test harnesses' fake.
    pub fn with_embedder(self, embedder: Box<dyn Embedder>) -> Self {
        self.encoder.inject(embedder);
        self
    }

    /// The store root, so a reader can open a connection of its own.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// The shared encoder, for a reader that needs the same warm weights.
    pub fn encoder(&self) -> Arc<Encoder> {
        Arc::clone(&self.encoder)
    }

    /// The settings a reader needs (`default_k`, `default_max_distance`) without the store lock.
    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// The vector space this store is configured for.
    ///
    /// The write path needs exactly two things from the embedder — the model id and the dimension, to
    /// fingerprint a document — and both are config. That is what lets a write run without loading
    /// 1.2 GB of weights (`ENCODE-WORKER.md`).
    pub fn vec_spec(&self) -> VecSpec {
        VecSpec::new(self.cfg.model.clone(), self.cfg.dim)
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

    /// Upsert a memory from a partial patch (SPEC §7): `None` fields keep their stored value, so one
    /// field can change without resending the row.
    ///
    /// **No encoder.** The document is queued, not embedded (`ENCODE-WORKER.md`), so a write holds
    /// nothing slower than a row insert — that coupling is what this whole design removes.
    pub fn memory_merge(&self, patch: &store::Patch) -> Result<store::MergeOutcome> {
        store::merge(&self.conn, &self.vec_spec(), patch)
    }

    /// Fetch one memory by key without counting a visit. Not public API: `fetch` is the read path on
    /// both doors, and the only caller is [`Self::fetch`] itself.
    pub(crate) fn show(&self, key: &str) -> Result<Memory> {
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
    /// point, backing `memory_get` on **both** doors: MCP's tool and `GET /memory/{key}`.
    ///
    /// This one WRITES the counter, so it belongs to the writer's session. The read door serves the
    /// row from a read-only connection and then bumps the count best-effort ([`Self::record_visit`]);
    /// a read must never wait for its own telemetry.
    pub fn fetch(&self, key: &str) -> Result<Memory> {
        let mut mem = self.show(key)?;
        mem.visits = store::record_visit(&self.conn, key)?;
        Ok(mem)
    }

    /// Count one read (`visits`) on this session's connection.
    pub fn record_visit(&self, key: &str) -> Result<i64> {
        store::record_visit(&self.conn, key)
    }

    /// Bump `times_useful` for many keys at once. Returns `(key, new_total)` for the keys that
    /// existed and the keys that did not, both in input order — a missing key is not an error.
    pub fn record_useful_many(&self, keys: &[String]) -> Result<store::UsefulSweep> {
        store::record_useful_many(&self.conn, keys)
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

    /// One key's versions, oldest first, WITHOUT bodies — enough to choose a version before paying
    /// for one (`store::version`). An empty list is a fact and not an error: a key with no history
    /// reads the same whether it never existed or never changed.
    ///
    /// Deliberately by KEY, never resolved through the `memory` row: a key that is GONE still has
    /// history, and that history is then the only copy of what it said.
    pub fn versions(&self, key: &str) -> Result<Vec<store::version::Version>> {
        store::version::history(&self.conn, key)
    }

    /// One version's snapshot, in the SAME shape as a live memory ([`Memory`]), so a caller handles a
    /// past state and a present one alike. `None` means no such version; the address need not exist.
    pub fn version(&self, key: &str, n: i64) -> Result<Option<Memory>> {
        match store::version::read(&self.conn, key, n)? {
            Some(snap) => Ok(Some(serde_json::from_value(snap)?)),
            None => Ok(None),
        }
    }

    /// The INDEX of history: every key that holds versions, with counts and an `orphan` flag. This is
    /// how a deleted key is found at all — nothing else in the store lists keys that are gone.
    pub fn version_index(&self, orphans_only: bool) -> Result<Vec<store::version::HistoryIndex>> {
        let mut rows = store::version::index(&self.conn)?;
        if orphans_only {
            rows.retain(|r| r.orphan);
        }
        Ok(rows)
    }

    /// How many versions one key holds — what a delete does NOT remove.
    pub fn version_count(&self, key: &str) -> Result<i64> {
        store::version::count_for(&self.conn, key)
    }

    /// Run a query, borrowing the encoder only when there is a phrase to embed. A read-only session
    /// serves the rows from its own connection, so a search does not wait for a write to finish.
    pub fn search(&self, q: &Query) -> Result<Bundle> {
        let needs_embedder = q
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        if needs_embedder {
            return self.encoder.with(|e| search::run(&self.conn, Some(e), q));
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

    /// Queue every document that needs (re-)embedding, and rebuild the derived indexes that take no
    /// model — the `[[link]]` graph, and the `vec0` table when the width changed (P9).
    ///
    /// **No encoder.** This used to recompute every vector under the store lock — ~45 minutes on a
    /// 291-row store, which made it a total write outage — and now schedules the work and returns.
    /// It backs the `memory_reembed` tool.
    pub fn reembed(&self) -> Result<maintain::Report> {
        maintain::reembed(&self.conn, &self.vec_spec(), &self.cfg.instruction)
    }

    /// The head of the encode queue, or `None` when nothing is waiting to be embedded.
    pub fn next_encode(
        &self,
        skip: &std::collections::HashSet<String>,
    ) -> Result<Option<encode::Entry>> {
        encode::next(&self.conn, skip)
    }

    /// Take one queued document **without embedding anything**: the read half of a job.
    ///
    /// Split from [`settle_encode`](Self::settle_encode) on purpose — the worker drops the store lock
    /// between the two, so the model call runs holding nothing at all. `ENCODE-WORKER.md` is the why.
    pub fn claim_encode(&self, keyslug: &str) -> Result<Option<encode::Claim>> {
        encode::claim(&self.conn, &self.vec_spec(), keyslug)
    }

    /// Store a claim's vector and settle its queue entry: the write half of a job.
    pub fn settle_encode(&self, claim: &encode::Claim, vector: &[f32]) -> Result<()> {
        encode::settle_encoded(&self.conn, claim, vector)
    }

    /// Queue every document that has no vector, at bulk priority, skipping the keyslugs a caller has
    /// just seen fail. Returns how many were queued.
    ///
    /// This is the queue's ONLY producer, and it is stateless on purpose: "what has no vector" is read
    /// off the index, so a rebuild, a crash, a dropped entry and a hand-deleted vector all arrive here
    /// as the same question. When nothing is missing it queues nothing and the worker idles.
    pub fn refill(&self, skip: &std::collections::HashSet<String>) -> Result<usize> {
        let mut queued = 0;
        for (keyslug, content) in encode::missing(&self.conn)? {
            if skip.contains(&keyslug) {
                continue;
            }
            encode::enqueue(&self.conn, &keyslug, &content, encode::PRIORITY_BULK)?;
            queued += 1;
        }
        Ok(queued)
    }

    /// Documents still waiting for a vector — the encoder's backlog.
    pub fn encode_pending(&self) -> Result<i64> {
        encode::pending(&self.conn)
    }

    /// The queue as a metric: backlog, exhausted entries, and the oldest wait (`store::encode::Counts`).
    /// The honest form of [`Self::encode_pending`], which is now its `pending` head.
    pub fn encode_counts(&self) -> Result<encode::Counts> {
        encode::counts(&self.conn)
    }

    /// Embed one queued document **now**, instead of leaving it to the worker: the `?wait=true` door.
    ///
    /// Deliberately the exception. It holds the store lock across the model call, which is precisely
    /// the coupling the queue exists to remove — so it is opt-in, and for a caller that genuinely
    /// cannot proceed until the row is searchable.
    /// The `?wait=true` door is MEMORY-shaped: its callers have just written a memory and hold its
    /// key, so the keyslug is built here rather than at every call site.
    pub fn encode_document(&self, key: &str) -> Result<bool> {
        let keyslug = crate::store::links::Kind::Memory.keyslug(key);
        self.encoder
            .with(|e| encode::encode_one(&self.conn, e, &self.vec_spec(), &keyslug))
    }

    /// Every row whose body links to `key` — the MCP `memory_backlinks` surface.
    ///
    /// A key with no row is [`Error::NotFound`], not an empty list: *no backlinks* and *no such
    /// memory* are different answers, and it is the second one that matters to a caller deciding
    /// whether a memory is safe to delete. The lookup goes through `store::get`, so the key is
    /// normalised exactly as it is everywhere else.
    pub fn backlinks(&self, key: &str) -> Result<Vec<links::Backlink>> {
        let mem = store::get(&self.conn, key)?.ok_or_else(|| Error::NotFound(key.to_string()))?;
        links::backlinks(&self.conn, &mem.key)
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

    /// Take or renew an atomic lease over `name` — the cross-machine claim.
    ///
    /// `acquire` is one SQL statement (`store::claim::acquire`), so a live lease held by another owner
    /// is refused and the same owner re-taking it renews. The store's versioning cannot give this: a
    /// claim built from `memory_get` + `memory_upsert` would be a racy read-modify-write.
    pub fn claim_acquire(
        &self,
        name: &str,
        owner: &str,
        ttl_seconds: Option<i64>,
    ) -> Result<claim::Acquire> {
        claim::acquire(&self.conn, name, owner, ttl_seconds)
    }

    /// Release a lease, but only your own. A non-owner is refused and told who holds it.
    pub fn claim_release(&self, name: &str, owner: &str) -> Result<claim::Release> {
        claim::release(&self.conn, name, owner)
    }

    /// The claims a reader can see: one by `name`, or every claim (expired rows included) when `name`
    /// is `None`. The audit view — an unexplained stale claim must be visible, not hidden.
    pub fn claims(&self, name: Option<&str>) -> Result<Vec<claim::Claim>> {
        match name {
            Some(n) => Ok(claim::get(&self.conn, n)?.into_iter().collect()),
            None => claim::list(&self.conn),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A fake encoder that takes a measurable moment, so "did the second caller wait?" is a fact and
    /// not an opinion.
    struct Slow;

    impl Embedder for Slow {
        fn dim(&self) -> usize {
            2
        }
        fn model_id(&self) -> &str {
            "slow-test"
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            // The read pays this too. Concurrent, the reader finishes at ~250ms; serialized it waits
            // out the writer first and finishes at ~400ms. The assert below separates those.
            std::thread::sleep(Duration::from_millis(200));
            Ok(vec![vec![0.0, 1.0]; texts.len()])
        }
    }

    /// The encoder must NOT serialize its callers.
    ///
    /// A write embedding a document can take seconds; a read embedding a query must not wait for it.
    /// That serialization was the whole stall, and putting a lock over the weights only moved it from
    /// the store to the model — which is why there is a `OnceLock` here and not a `Mutex`. This test
    /// is what keeps it one.
    #[test]
    fn the_encoder_does_not_serialize_its_callers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_root(dir.path());
        paths.ensure().expect("store root");
        let cfg = Config::default();
        let spec = crate::model::Spec::from_config(&cfg, &paths);
        let encoder = Encoder::new(spec, cfg);
        encoder.inject(Box::new(Slow));

        let writer = Arc::clone(&encoder);
        let writing =
            std::thread::spawn(move || writer.with(|e| e.embed_one("a document being written")));
        std::thread::sleep(Duration::from_millis(50));

        let started = Instant::now();
        encoder
            .with(|e| e.embed_one("a query being searched"))
            .expect("the read's own embed");
        let waited = started.elapsed();

        writing
            .join()
            .expect("the writer's thread")
            .expect("its embed");
        assert!(
            waited < Duration::from_millis(350),
            "the read waited {waited:?} for the write's embed: the encoder is serializing them again"
        );
    }
}
