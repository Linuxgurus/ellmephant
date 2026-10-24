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

use rusqlite::{Connection, OptionalExtension};

use crate::config::{Config, Paths};
use crate::db;
use crate::embed::{Embedder, VecSpec};
use crate::error::{Error, Result};
use crate::model;
use crate::search::{self, Bundle, Query};
use crate::store::{self, Memory, claim, encode, links, mail, maintain, project};

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
    /// WHO this session reads as. The CLI holds the file, so it is [`Reader::everything`] by default
    /// — the honest limit of the private tier (#54). A DOOR replaces it with the connection's proven
    /// pen, and then every read below filters through it.
    reader: store::audience::Reader,
    /// ★ AN AUDIT IS A WRITE (#129). A read-only session still has to be able to RECORD that it
    /// crossed a boundary, and a read-only connection cannot. Opened lazily, on the rare act that
    /// needs it, so the ordinary read path pays nothing for the audit and the audit still happens
    /// where it matters. `None` until then, and `Some` for a writer session, which can just use its
    /// own connection.
    audit_conn: std::cell::RefCell<Option<Connection>>,
    /// True for a door's read-only session: it may READ the store and may still WRITE its audit.
    read_only: bool,
}

impl Session {
    /// Open (creating if needed) the store and hold the connection.
    pub fn open(paths: Paths, cfg: Config, spec: model::Spec) -> Result<Self> {
        let conn = db::open_with(&paths, &cfg)?;
        let encoder = Encoder::new(spec, cfg.clone());
        Ok(Self {
            paths,
            cfg,
            conn,
            encoder,
            reader: store::audience::Reader::everything(),
            audit_conn: std::cell::RefCell::new(None),
            read_only: false,
        })
    }

    /// A READ-ONLY session: its own connection, the shared encoder, and no store lock. This is how a
    /// read is served while a write is in flight — the writer holds its session for the whole write,
    /// embedding included, so a read that queued on it would wait out the write.
    pub fn read_only(paths: &Paths, cfg: Config, encoder: Arc<Encoder>) -> Result<Self> {
        Self::read_only_as(paths, cfg, encoder, store::audience::Reader::everything())
    }

    /// A read-only session that reads as one pen (or as nobody): the door's shape.
    pub fn read_only_as(
        paths: &Paths,
        cfg: Config,
        encoder: Arc<Encoder>,
        reader: store::audience::Reader,
    ) -> Result<Self> {
        let conn = db::open_read_only(paths)?;
        // ★ PARENTAGE IS THE GRANT, resolved at use (#109): the reader reaches down into its own
        // subtree, so an ancestor may read what a descendant addressed to itself — the doctor's
        // licence, and what makes cluster-wide hygiene possible without being the author.
        // …and a held session widens it to the pen being borrowed, for as long as the row says so.
        let reader = reader.in_tree(&conn).with_borrowed(&conn);
        Ok(Self {
            paths: paths.clone(),
            cfg,
            conn,
            encoder,
            audit_conn: std::cell::RefCell::new(None),
            read_only: true,
            reader,
        })
    }

    /// The reader this session sees with.
    pub fn reader(&self) -> &store::audience::Reader {
        &self.reader
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
        store::merge(&self.conn, patch)
    }

    /// Fetch one memory by key without counting a visit. Not public API: `fetch` is the read path on
    /// both doors, and the only caller is [`Self::fetch`] itself.
    pub(crate) fn show(&self, key: &str) -> Result<Memory> {
        // ABSENCE, not refusal: a row this reader may not see is answered exactly as a key that was
        // never written. A distinct "private" error would confirm the row exists, which is the
        // disclosure the tier is for.
        match store::get(&self.conn, key)? {
            Some(mem) if self.reader.sees(&mem.audience) => {
                // ★ A READ THAT CROSSED A BOUNDARY IS RECORDED (#129). An ancestor reading down its
                // subtree is the one act the tier allowed and did not log; best effort, never fatal.
                if self.reader.crosses(&mem.audience) {
                    // ★ WHICH MECHANISM (adversarial review, 2026-10-24): reached by parentage, or by a
                    // session this pen was lent? Recording both as `subtree` would make the log claim an
                    // ancestor read for a read that no ancestry explains.
                    let why = if self.reader.is_borrowed(&mem.audience) {
                        store::audit::Why::Borrowed
                    } else {
                        store::audit::Why::Subtree
                    };
                    self.audit(Some(key), why, None);
                }
                Ok(mem)
            }
            _ => Err(Error::NotFound(key.to_string())),
        }
    }

    /// May `proven` be credited as `claimed` on a write? (Live lend required.)
    pub fn may_act_as(&self, proven: &str, claimed: &str) -> Result<()> {
        store::may_act_as(&self.conn, proven, claimed)
    }

    /// Lend this pen's own authority down to a descendant, for `secs` — it expires and can be revoked.
    pub fn lend(&self, powerful: &str, weak: &str, secs: i64) -> Result<store::sudo::Session> {
        store::sudo::lend(&self.conn, powerful, weak, secs)
    }

    /// Borrow `powerful`'s authority for `secs` (needs a policy row).
    pub fn sudo(&self, weak: &str, powerful: &str, secs: i64) -> Result<store::sudo::Session> {
        store::sudo::sudo(&self.conn, weak, powerful, secs)
    }

    /// Pass a live session down to a descendant.
    pub fn sudo_pass(
        &self,
        weak: &str,
        powerful: &str,
        to: &str,
        secs: i64,
    ) -> Result<store::sudo::Session> {
        store::sudo::pass(&self.conn, weak, powerful, to, secs)
    }

    /// End a session and its pass-down chain.
    pub fn sudo_release(&self, weak: &str, powerful: &str) -> Result<usize> {
        store::sudo::release(&self.conn, weak, powerful)
    }

    /// Who holds what, right now — all pens, or one.
    pub fn sudo_sessions(&self, weak: Option<&str>) -> Result<Vec<store::sudo::Session>> {
        store::sudo::live_sessions(&self.conn, weak)
    }

    /// End a session for `target` (defaulting to the caller) — an ancestor may end a descendant's.
    pub fn sudo_release_of(
        &self,
        caller: &str,
        target: Option<&str>,
        powerful: &str,
    ) -> Result<usize> {
        match target {
            Some(pen) => store::sudo::release_by(&self.conn, caller, pen, powerful),
            None => store::sudo::release(&self.conn, caller, powerful),
        }
    }

    /// Mint a pen under `caller` — the runtime door.
    pub fn mint(&self, caller: &str, name: &str, kind: &str) -> Result<store::Minted> {
        store::mint(&self.conn, caller, name, kind)
    }

    /// One pen as `viewer` sees it — the tree through this session's connection.
    pub fn whois(&self, viewer: &str, pen: &str) -> Result<store::Whois> {
        store::whois(&self.conn, viewer, pen)
    }

    /// What this row's owner is TO THE READER, in the settled letters (`PS "uncle"`, `ME`, …).
    ///
    /// ★ A NAME IS AN ADDRESS, and this is the rest of the sentence: `owner` says who answers for a
    /// row, and the walk says what that pen is to the one asking (James, 2026-10-23). `None` when
    /// there is no owner (the commons), no proven reader, or no common root — absence, never a guess.
    pub fn owner_relation(&self, mem: &Memory) -> Option<String> {
        let viewer = self.reader.pen_name()?;
        let owner = mem.owner.as_deref()?;
        store::relation::relation(&self.conn, viewer, owner)
            .ok()
            .flatten()
            .map(|k| k.gloss())
    }

    /// Retire one memory into another: repoint every reference, then delete the source. The prose
    /// fold is the caller's — this never touches `into`'s body. No encoder: the graph is not
    /// embedded. (Distinct from [`memory_merge`](Self::memory_merge), which is the *field* patch
    /// behind `memory_upsert`.)
    pub fn merge_into(
        &self,
        from: &str,
        into: &str,
        acted_by: Option<&str>,
    ) -> Result<links::MergeInto> {
        links::merge_into(&self.conn, from, into, acted_by)
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
        store::dupes::find_seen(&self.conn, &self.reader, max_distance, limit)
    }

    /// Rows nobody owns, newest first — the list behind the counts, filtered by this session's reader.
    pub fn unowned(&self, limit: i64) -> Result<Vec<store::Unowned>> {
        store::unowned(&self.conn, &self.reader, limit)
    }

    /// Delete one memory by key; `false` means there was nothing to delete.
    ///
    /// `acted_by` is the proven pen the door is acting for: a delete is a write of absence, so it
    /// takes the same reach as an edit and leaves a ledger row saying who removed it.
    pub fn rm(&self, key: &str, acted_by: Option<&str>) -> Result<bool> {
        store::delete(&self.conn, key, acted_by)
    }

    /// Take responsibility for a row (`memory_adopt`). A finger's parent is what owns, so this
    /// adopts as the parent.
    pub fn adopt(&self, key: &str, pen: &str, force: bool) -> Result<()> {
        store::adopt(&self.conn, key, pen, force)
    }

    /// One key's versions, oldest first, WITHOUT bodies — enough to choose a version before paying
    /// for one (`store::version`). An empty list is a fact and not an error: a key with no history
    /// reads the same whether it never existed or never changed.
    ///
    /// Deliberately by KEY, never resolved through the `memory` row: a key that is GONE still has
    /// history, and that history is then the only copy of what it said.
    pub fn versions(&self, key: &str) -> Result<Vec<store::version::Version>> {
        self.version_guard(key)?;
        store::version::history(&self.conn, key)
    }

    /// ★ VERSIONS HOLD THE BODY, so history is gated by the row's CURRENT class (the decision #54
    /// asked to state rather than discover): a note that went private takes its past with it. A key
    /// with no live row is classed by its NEWEST snapshot, because that is the last thing it said
    /// about who could read it — and a private note's history must not outlive the note in public.
    fn version_guard(&self, key: &str) -> Result<()> {
        let class: Option<String> = match store::get(&self.conn, key)? {
            Some(mem) => Some(mem.audience),
            None => store::version::read_latest(&self.conn, key)?.and_then(|snap| {
                snap.get("audience")
                    .and_then(|a| a.as_str())
                    .map(str::to_string)
            }),
        };
        match class {
            Some(audience) if !self.reader.sees(&audience) => Err(Error::NotFound(key.to_string())),
            Some(audience) => {
                // ★ A PAST STATE IS STILL THE ROW (found reviewing #129). `version()` and `versions()`
                // both come through here, and both can hand an ancestor the contents of a descendant's
                // private row — so the boundary is crossed on this path too and must be recorded, or
                // the audit covers `memory_get` and nothing else.
                if self.reader.crosses(&audience) {
                    self.audit(
                        Some(key),
                        store::audit::Why::Subtree,
                        Some("version/history"),
                    );
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// One version's snapshot, in the SAME shape as a live memory ([`Memory`]), so a caller handles a
    /// past state and a present one alike. `None` means no such version; the address need not exist.
    pub fn version(&self, key: &str, n: i64) -> Result<Option<Memory>> {
        self.version_guard(key)?;
        match store::version::read(&self.conn, key, n)? {
            Some(snap) => Ok(Some(serde_json::from_value(snap)?)),
            None => Ok(None),
        }
    }

    /// The INDEX of history: every key that holds versions, with counts and an `orphan` flag. This is
    /// how a deleted key is found at all — nothing else in the store lists keys that are gone.
    pub fn version_index(&self, orphans_only: bool) -> Result<Vec<store::version::HistoryIndex>> {
        let mut rows = store::version::index(&self.conn)?;
        rows.retain(|r| self.version_guard(&r.key).is_ok());
        if orphans_only {
            rows.retain(|r| r.orphan);
        }
        Ok(rows)
    }

    /// How many versions one key holds — what a delete does NOT remove.
    pub fn version_count(&self, key: &str) -> Result<i64> {
        store::version::count_for(&self.conn, key)
    }

    /// Does this connection see a row for `key`? One boolean, and on a miss it is the one that splits
    /// the space in half: reader-only blindness (WAL, snapshot, path) versus the store not having it.
    pub fn exists(&self, key: &str) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM memory WHERE key = ?1",
                rusqlite::params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// The facts a MISS needs to be evidence rather than an assertion (soma/ellmephant#57).
    ///
    /// `memory_get` answering "no such memory" makes a claim about the whole store, and a bare
    /// refusal gives the reader nothing to test: on 2026-10-21 a live, versioned key was reported
    /// absent for 42 minutes and nobody could tell "wrong store" from "deleted" from "stale reader".
    /// These three numbers are the difference, and they cost one indexed query each.
    pub fn miss_facts(&self, key: &str) -> Result<MissFacts> {
        // ★ A DIAGNOSTIC IS A READ. "It existed 42 minutes ago" is exactly the sentence the tier
        // exists to prevent, so the history half is gated by the same rule as `memory_history`, and
        // the store count is the reader's count, not the file's.
        let (predicate, binds) = self.reader.sql();
        let memories: i64 = self.conn.query_row(
            &format!("SELECT count(*) FROM memory WHERE {predicate}"),
            rusqlite::params_from_iter(binds.iter()),
            |r| r.get(0),
        )?;
        let may_see_history = self.version_guard(key).is_ok();
        let versions = if may_see_history {
            store::version::count_for(&self.conn, key)?
        } else {
            0
        };
        let newest: Option<String> = if may_see_history {
            self.conn.query_row(
                "SELECT MAX(written_at) FROM memory_version WHERE type = 'memory' AND key = ?1",
                rusqlite::params![key],
                |r| r.get(0),
            )?
        } else {
            None
        };
        Ok(MissFacts {
            store: self.paths.db().display().to_string(),
            memories,
            schema: crate::db::migrate::version(&self.conn)?,
            versions,
            newest,
        })
    }

    /// Run a query, borrowing the encoder only when there is a phrase to embed. A read-only session
    /// serves the rows from its own connection, so a search does not wait for a write to finish.
    /// Vectors held for one document type — what `semantic_search` checks before promising a type.
    pub fn vectors_of(&self, kind: &str) -> Result<i64> {
        crate::index::vec::count_of(&self.conn, kind)
    }

    /// Search the **tombstone** index: deleted documents, by meaning.
    ///
    /// A different table rather than a filtered view, which is what makes "unreachable by a normal
    /// search" structural: no ordinary query names this table, so no hit can leak into one, and the
    /// `k` a caller asks for is exactly the `k` they get.
    pub fn search_deleted(
        &self,
        text: &str,
        k: usize,
        max_distance: Option<f32>,
    ) -> Result<Vec<crate::index::vec::Tombstone>> {
        self.encoder.with(|e| {
            let vector = e.embed_query(text)?;
            crate::index::vec::knn_deleted(
                &self.conn,
                &vector,
                &crate::index::vec::Knn { k, max_distance },
            )
        })
    }

    pub fn search(&self, q: &Query) -> Result<Bundle> {
        let needs_embedder = q
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        let bundle = if needs_embedder {
            self.encoder
                .with(|e| search::run(&self.conn, Some(e), &self.reader, q))?
        } else {
            search::run(&self.conn, None, &self.reader, q)?
        };
        // ★ A SEARCH IS THE COMMON WAY AN ANCESTOR LOOKS INTO ITS SUBTREE, so it is the common way a
        // boundary gets crossed — and one row per hit would make the audit bigger than the store.
        // One row per SEARCH, carrying how many of the hits were somebody else's private rows and a
        // small sample, is the record of the ACT rather than of its results.
        self.record_search_boundary(&bundle);
        Ok(bundle)
    }

    /// Record a read that crossed a boundary, wherever this session can write.
    ///
    /// ★ THE READ PATH IS READ-ONLY BY DESIGN, and the audit must still land: so the record goes on
    /// the session's own connection when it can write, and otherwise on a lazily-opened writable one.
    /// Best effort — a read never fails because its record could not be written.
    fn audit(&self, key: Option<&str>, why: store::audit::Why, detail: Option<&str>) {
        let Some(pen) = self.reader.pen_name() else {
            return;
        };
        if !self.read_only {
            store::audit::record(&self.conn, pen, key, why, detail);
            return;
        }
        let mut slot = self.audit_conn.borrow_mut();
        if slot.is_none() {
            match db::open_with(&self.paths, &self.cfg) {
                Ok(c) => *slot = Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "could not open a connection to record a boundary read");
                    return;
                }
            }
        }
        if let Some(conn) = slot.as_ref() {
            store::audit::record(conn, pen, key, why, detail);
        }
    }

    /// Record the boundary a search crossed, if it crossed one. Best effort, never fails the read.
    fn record_search_boundary(&self, bundle: &Bundle) {
        if self.reader.pen_name().is_none() || self.reader.down() == 0 || bundle.hits.is_empty() {
            return;
        }
        let keys: Vec<&str> = bundle.hits.iter().map(|h| h.key.as_str()).collect();
        let crossed = match store::audience::Reader::crossed_keys(&self.conn, &self.reader, &keys) {
            Ok(k) => k,
            Err(_) => return,
        };
        if crossed.is_empty() {
            return;
        }
        let sample: Vec<&str> = crossed.iter().take(5).map(String::as_str).collect();
        let detail = format!(
            "{} of {} hit(s): {}",
            crossed.len(),
            keys.len(),
            sample.join(", ")
        );
        self.audit(None, store::audit::Why::Search, Some(&detail));
    }

    /// A read-only digest of the store (`stats`); `db_bytes` comes from the store file, so the
    /// MCP surface reports the real size rather than a zero the caller was expected to fill.
    /// `with_tags` additionally fills the full tag vocabulary — the digest stays lean without it.
    pub fn stats(&self, with_tags: bool) -> Result<store::stats::Stats> {
        let mut stats = store::stats::compute_seen(&self.conn, &self.reader)?;
        if with_tags {
            stats.all_tags = store::stats::vocabulary_seen(&self.conn, &self.reader)?;
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

    /// Rebuild the `[[link]]` graph from every body — no model, no queue, milliseconds.
    ///
    /// The graph is derived but computed at *write* time, so any path that rewrites a body without
    /// re-deriving (a textual rewrite like a rename/merge `replace()`, a parser change, an older
    /// binary) leaves edges the rows no longer imply. `memory_reembed` fixes that as a side effect
    /// and costs a model load plus an hour of worker time; this is the broom for when only the graph
    /// moved. It backs the `memory_reparse` tool.
    pub fn reparse(&self) -> Result<maintain::Reparse> {
        maintain::reparse(&self.conn)
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
        encode::claim(&self.conn, keyslug)
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
            .with(|e| encode::encode_one(&self.conn, e, &keyslug))
    }

    /// Every row whose body links to `key` — the MCP `memory_backlinks` surface.
    ///
    /// A key with no row is [`Error::NotFound`], not an empty list: *no backlinks* and *no such
    /// memory* are different answers, and it is the second one that matters to a caller deciding
    /// whether a memory is safe to delete. The lookup goes through `store::get`, so the key is
    /// normalised exactly as it is everywhere else.
    pub fn backlinks(&self, key: &str) -> Result<Vec<links::Backlink>> {
        // The TARGET is gated first: asking for backlinks of a row you cannot see must not tell you
        // whether the row exists. Then the SOURCES are filtered — a private note that names this key
        // is not a note this reader can be shown.
        let mem = self.show(key)?;
        links::backlinks_seen(&self.conn, &mem.key, &self.reader)
    }

    /// Every broken `[[…]]` edge, one row per referrer — the finder behind `memory_dangling`.
    /// [`crate::store::stats::Stats::dangling`] counts DISTINCT missing targets; this names them.
    pub fn dangling(&self) -> Result<Vec<links::DanglingEdge>> {
        links::dangling_edges(&self.conn)
    }

    /// Close the books on a rebuild: the actual duration, beside the estimate (`#76`).
    pub fn note_rebuild_finished(&self) -> Result<()> {
        maintain::note_rebuild_finished(&self.conn)
    }

    /// Upsert a project from a partial patch: `None` fields keep their stored value. No encoder — a
    /// project is structure, not a document.
    pub fn project_merge(&self, patch: &project::Patch) -> Result<project::MergeOutcome> {
        project::merge(&self.conn, patch)
    }

    /// One project with both relations resolved — the "stand inside it" read. `hydrate` decides how
    /// much of each preload memory's body to carry.
    pub fn project_view(&self, key: &str, hydrate: project::Hydrate) -> Result<project::View> {
        project::view_seen(&self.conn, key, hydrate, &self.reader)?
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

    /// Write one letter into `recipient`'s box. Nothing is delivered: this is the whole event.
    pub fn mail_send(
        &self,
        sender: &str,
        recipient: &str,
        subject: &str,
        body: &str,
        encoding: &str,
    ) -> Result<mail::Envelope> {
        mail::send(&self.conn, sender, recipient, subject, body, encoding)
    }

    /// Everything in `recipient`'s box after `since`, oldest first, bodies omitted.
    pub fn mail_inbox(
        &self,
        recipient: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<mail::Envelope>> {
        mail::inbox(&self.conn, recipient, since, limit)
    }

    /// The newest `seq` in a box, 0 when empty — the free "is anything there?" a hand's tick asks.
    pub fn mail_head(&self, recipient: &str) -> Result<i64> {
        mail::head(&self.conn, recipient)
    }

    /// One letter whole.
    pub fn mail_read(&self, seq: i64) -> Result<Option<mail::Letter>> {
        mail::read(&self.conn, seq)
    }

    /// Drop letters nobody ever read. ★ Letters expire; memories do not.
    pub fn mail_prune(&self, keep_days: i64) -> Result<usize> {
        mail::prune(&self.conn, keep_days)
    }

    /// The reader's place in a box (`None` = never advanced, so the whole box is unread).
    pub fn mail_cursor(&self, reader: &str, recipient: &str) -> Result<Option<mail::Cursor>> {
        mail::cursor(&self.conn, reader, recipient)
    }

    /// Advance the reader's own cursor — the commit of an inbox read.
    pub fn mail_advance(&self, reader: &str, recipient: &str, seq: i64) -> Result<mail::Cursor> {
        mail::advance(&self.conn, reader, recipient, seq)
    }
}

/// What a miss can prove without a second round trip — see [`Session::miss_facts`].
#[derive(Debug, Clone)]
pub struct MissFacts {
    /// The store file the reader actually opened. The first hypothesis in #57 was "a different file".
    pub store: String,
    /// How many memories that file holds: zero means an empty/other store, not a missing key.
    pub memories: i64,
    /// Schema version of that file, so a stale reader or an unmigrated copy is visible.
    pub schema: i32,
    /// Versions recorded for this key — history outlives a delete, so >0 means "deleted, not absent".
    pub versions: i64,
    /// When the newest version was written, when there is one.
    pub newest: Option<String>,
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
