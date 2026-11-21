use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};

use crate::config::{Config, Paths};
use crate::db;
use crate::embed::{Embedder, VecSpec};
use crate::error::{Error, Result};
use crate::model;
use crate::search::{self, Bundle, Query};
use crate::store::{self, Memory, channel, claim, config, encode, links, mail, maintain, project};

pub struct Encoder {
    spec: model::Spec,
    cfg: Config,
    inner: std::sync::OnceLock<Box<dyn Embedder>>,
    loading: std::sync::Mutex<()>,
}

impl Encoder {
    pub fn new(spec: model::Spec, cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            spec,
            cfg,
            inner: std::sync::OnceLock::new(),
            loading: std::sync::Mutex::new(()),
        })
    }

    pub fn with<T>(&self, f: impl FnOnce(&dyn Embedder) -> Result<T>) -> Result<T> {
        if let Some(embedder) = self.inner.get() {
            return f(embedder.as_ref());
        }
        {
            let _loading = self.loading.lock().unwrap_or_else(|e| e.into_inner());
            if self.inner.get().is_none() {
                tracing::info!(
                    model = %self.cfg.model,
                    device = %self.cfg.device,
                    precision = %self.cfg.precision,
                    dim = self.cfg.dim,
                    "loading encoder"
                );
                let started = std::time::Instant::now();
                let loaded: Box<dyn Embedder> =
                    Box::new(crate::embed::load_embedder(&self.spec, &self.cfg)?);
                let _ = self.inner.set(loaded);
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "encoder loaded"
                );
            }
        }
        f(self.inner.get().expect("set just above").as_ref())
    }

    pub fn inject(&self, embedder: Box<dyn Embedder>) {
        let _ = self.inner.set(embedder);
    }
}

fn reader_allows(conn: &Connection, reader: &crate::store::audience::Reader, key: &str) -> bool {
    let class = match store::get(conn, key) {
        Ok(Some(mem)) => Some(mem.audience),
        _ => store::version::read_latest(conn, key)
            .ok()
            .flatten()
            .and_then(|snap| {
                snap.get("audience")
                    .and_then(|a| a.as_str())
                    .map(str::to_string)
            }),
    };
    match class {
        Some(a) => reader.sees(&a),
        None => false,
    }
}

pub struct Session {
    paths: Paths,
    cfg: Config,
    conn: Connection,
    encoder: Arc<Encoder>,
    reader: store::audience::Reader,
    audit_conn: std::cell::RefCell<Option<Connection>>,
    read_only: bool,
}

impl Session {
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

    pub fn read_only(paths: &Paths, cfg: Config, encoder: Arc<Encoder>) -> Result<Self> {
        Self::read_only_as(paths, cfg, encoder, store::audience::Reader::everything())
    }

    pub fn read_only_as(
        paths: &Paths,
        cfg: Config,
        encoder: Arc<Encoder>,
        reader: store::audience::Reader,
    ) -> Result<Self> {
        let conn = db::open_read_only(paths)?;
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

    pub fn reader(&self) -> &store::audience::Reader {
        &self.reader
    }

    pub fn with_embedder(self, embedder: Box<dyn Embedder>) -> Self {
        self.encoder.inject(embedder);
        self
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn encoder(&self) -> Arc<Encoder> {
        Arc::clone(&self.encoder)
    }

    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    pub fn vec_spec(&self) -> VecSpec {
        VecSpec::new(self.cfg.model.clone(), self.cfg.dim)
    }

    pub fn default_k(&self) -> usize {
        self.cfg.default_k
    }

    pub fn default_max_distance(&self) -> Option<f32> {
        self.cfg.default_max_distance
    }

    pub fn memory_merge(&self, patch: &store::Patch) -> Result<store::MergeOutcome> {
        store::merge(&self.conn, patch)
    }

    pub(crate) fn show(&self, key: &str) -> Result<Memory> {
        match store::get(&self.conn, key)? {
            Some(mem) if self.reader.sees(&mem.audience) => {
                if self.reader.crosses(&mem.audience) {
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

    pub fn may_act_as(&self, proven: &str, claimed: &str) -> Result<()> {
        store::may_act_as(&self.conn, proven, claimed)
    }

    pub fn lend(&self, powerful: &str, weak: &str, secs: i64) -> Result<store::sudo::Session> {
        store::sudo::lend(&self.conn, powerful, weak, secs)
    }

    pub fn sudo(&self, weak: &str, powerful: &str, secs: i64) -> Result<store::sudo::Session> {
        store::sudo::sudo(&self.conn, weak, powerful, secs)
    }

    pub fn sudo_pass(
        &self,
        weak: &str,
        powerful: &str,
        to: &str,
        secs: i64,
    ) -> Result<store::sudo::Session> {
        store::sudo::pass(&self.conn, weak, powerful, to, secs)
    }

    pub fn sudo_release(&self, weak: &str, powerful: &str) -> Result<usize> {
        store::sudo::release(&self.conn, weak, powerful)
    }

    pub fn sudo_sessions(&self, weak: Option<&str>) -> Result<Vec<store::sudo::Session>> {
        store::sudo::live_sessions(&self.conn, weak)
    }

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

    pub fn mint(&self, caller: &str, name: &str, kind: &str) -> Result<store::Minted> {
        store::mint(&self.conn, caller, name, kind)
    }

    pub fn rotate_credential(&self, caller: &str, target: &str) -> Result<String> {
        store::credential::rotate(&self.conn, Some(caller), target)
    }

    pub fn whois(&self, viewer: &str, pen: &str) -> Result<store::Whois> {
        store::whois(&self.conn, viewer, pen)
    }

    pub fn owner_relation(&self, mem: &Memory) -> Option<String> {
        let viewer = self.reader.pen_name()?;
        let owner = mem.owner.as_deref()?;
        store::relation::relation(&self.conn, viewer, owner)
            .ok()
            .flatten()
            .map(|k| k.gloss())
    }

    pub fn merge_into(
        &self,
        from: &str,
        into: &str,
        acted_by: Option<&str>,
    ) -> Result<links::MergeInto> {
        links::merge_into(&self.conn, from, into, acted_by)
    }

    pub fn append(
        &self,
        key: &str,
        line: &str,
        acted_by: Option<&str>,
        force: bool,
    ) -> Result<store::append::Appended> {
        store::append::append(&self.conn, key, line, acted_by, force)
    }

    pub fn fold_into(
        &self,
        from: &str,
        into: &str,
        acted_by: Option<&str>,
    ) -> Result<links::FoldOutcome> {
        links::fold_into(&self.conn, from, into, acted_by)
    }

    pub fn fetch(&self, key: &str) -> Result<Memory> {
        let mut mem = self.show(key)?;
        mem.visits = store::record_visit(&self.conn, key)?;
        Ok(mem)
    }

    pub fn record_visit(&self, key: &str) -> Result<i64> {
        store::record_visit(&self.conn, key)
    }

    pub fn record_useful_many(&self, keys: &[String]) -> Result<store::UsefulSweep> {
        store::record_useful_many(&self.conn, keys)
    }

    pub fn dupes(&self, max_distance: f32, limit: usize) -> Result<Vec<store::dupes::DupePair>> {
        store::dupes::find_seen(&self.conn, &self.reader, max_distance, limit)
    }

    pub fn dupes_plan(&self, max_distance: f32, limit: usize) -> Result<store::dupes::MergePlan> {
        store::dupes::plan_seen(&self.conn, &self.reader, max_distance, limit)
    }

    pub fn unowned(&self, limit: i64) -> Result<Vec<store::Unowned>> {
        store::unowned(&self.conn, &self.reader, limit)
    }

    pub fn rm(&self, key: &str, acted_by: Option<&str>) -> Result<bool> {
        store::delete(&self.conn, key, acted_by)
    }

    pub fn adopt(&self, key: &str, pen: &str, force: bool) -> Result<()> {
        store::adopt(&self.conn, key, pen, force)
    }

    pub fn assign(&self, key: &str, actor: &str, to: &str, force: bool) -> Result<()> {
        store::assign(&self.conn, key, actor, to, force)
    }

    pub fn versions(&self, key: &str) -> Result<Vec<store::version::Version>> {
        self.version_guard(key)?;
        store::version::history(&self.conn, key)
    }

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

    pub fn version(&self, key: &str, n: i64) -> Result<Option<Memory>> {
        self.version_guard(key)?;
        match store::version::read(&self.conn, key, n)? {
            Some(snap) => Ok(Some(serde_json::from_value(snap)?)),
            None => Ok(None),
        }
    }

    pub fn version_index(&self, orphans_only: bool) -> Result<Vec<store::version::HistoryIndex>> {
        let mut rows = store::version::index(&self.conn)?;
        rows.retain(|r| self.version_guard(&r.key).is_ok());
        if orphans_only {
            rows.retain(|r| r.orphan);
        }
        Ok(rows)
    }

    pub fn erase_history(&self, key: &str) -> Result<usize> {
        if store::get(&self.conn, key)?.is_some() {
            return Err(Error::Invalid(format!(
                "`{key}` still exists: its history is the only copy of what it said. Delete the                  memory first (which snapshots it), then erase its history."
            )));
        }
        store::version::erase(&self.conn, key)
    }

    pub fn history_search(
        &self,
        phrase: &str,
        limit: usize,
    ) -> Result<Vec<store::version::HistoryMatch>> {
        let reader = &self.reader;
        store::version::search_visible(&self.conn, phrase, limit, |key| {
            reader_allows(&self.conn, reader, key)
        })
    }

    pub fn erase_orphan_history(&self) -> Result<(usize, usize)> {
        store::version::erase_orphans(&self.conn)
    }

    pub fn version_count(&self, key: &str) -> Result<i64> {
        store::version::count_for(&self.conn, key)
    }

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

    pub fn self_facts(&self, key: &str) -> Result<Option<(String, String)>> {
        let row: Option<(Option<i64>, String)> = self
            .conn
            .query_row(
                "SELECT owner, audience FROM memory WHERE key = ?1",
                rusqlite::params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((Some(owner_id), audience)) = row else {
            return Ok(None);
        };
        let name: String = self.conn.query_row(
            "SELECT name FROM pen WHERE id = ?1",
            rusqlite::params![owner_id],
            |r| r.get(0),
        )?;
        Ok(Some((name, audience)))
    }

    pub fn miss_facts(&self, key: &str) -> Result<MissFacts> {
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

    pub fn vectors_of(&self, kind: &str) -> Result<i64> {
        crate::index::vec::count_of(&self.conn, kind)
    }

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
        self.record_search_boundary(&bundle);
        Ok(bundle)
    }

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

    pub fn stats(&self, with_tags: bool) -> Result<store::stats::Stats> {
        let mut stats = store::stats::compute_seen(&self.conn, &self.reader)?;
        if with_tags {
            stats.all_tags = store::stats::vocabulary_seen(&self.conn, &self.reader)?;
        }
        Ok(stats)
    }

    pub fn health(&self) -> Result<store::health::Health> {
        store::health::compute_seen(&self.conn, &self.reader)
    }

    pub fn reembed(&self) -> Result<maintain::Report> {
        maintain::reembed(&self.conn, &self.vec_spec(), &self.cfg.instruction)
    }

    pub fn reparse(&self) -> Result<maintain::Reparse> {
        maintain::reparse(&self.conn)
    }

    pub fn next_encode(
        &self,
        skip: &std::collections::HashSet<String>,
    ) -> Result<Option<encode::Entry>> {
        encode::next(&self.conn, skip)
    }

    pub fn claim_encode(&self, keyslug: &str) -> Result<Option<encode::Claim>> {
        encode::claim(&self.conn, keyslug)
    }

    pub fn settle_encode(&self, claim: &encode::Claim, vector: &[f32]) -> Result<()> {
        encode::settle_encoded(&self.conn, claim, vector)
    }

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

    pub fn encode_pending(&self) -> Result<i64> {
        encode::pending(&self.conn)
    }

    pub fn encode_counts(&self) -> Result<encode::Counts> {
        encode::counts(&self.conn)
    }

    pub fn encode_document(&self, key: &str) -> Result<bool> {
        let keyslug = crate::store::links::Kind::Memory.keyslug(key);
        self.encoder
            .with(|e| encode::encode_one(&self.conn, e, &keyslug))
    }

    pub fn backlinks(&self, key: &str) -> Result<Vec<links::Backlink>> {
        let mem = self.show(key)?;
        links::backlinks_seen(&self.conn, &mem.key, &self.reader)
    }

    pub fn dangling(&self) -> Result<Vec<links::DanglingEdge>> {
        links::dangling_edges(&self.conn)
    }

    pub fn note_rebuild_finished(&self) -> Result<()> {
        maintain::note_rebuild_finished(&self.conn)
    }

    pub fn project_merge(&self, patch: &project::Patch) -> Result<project::MergeOutcome> {
        project::merge(&self.conn, patch)
    }

    pub fn project_view(&self, key: &str, hydrate: project::Hydrate) -> Result<project::View> {
        project::view_seen(&self.conn, key, hydrate, &self.reader)?
            .ok_or_else(|| Error::NoProject(project::normalize_key(key)))
    }

    pub fn project_list(&self, filter: &project::Filter) -> Result<Vec<project::Project>> {
        project::list(&self.conn, filter)
    }

    pub fn claim_acquire(
        &self,
        name: &str,
        owner: &str,
        ttl_seconds: Option<i64>,
    ) -> Result<claim::Acquire> {
        claim::acquire(&self.conn, name, owner, ttl_seconds)
    }

    pub fn claim_release(
        &self,
        name: &str,
        actor: &str,
        reason: Option<&str>,
    ) -> Result<claim::Release> {
        claim::release_by(&self.conn, name, actor, reason)
    }

    pub fn claim_revocations(
        &self,
        name: Option<&str>,
        limit: i64,
    ) -> Result<Vec<claim::Revocation>> {
        claim::revocations(&self.conn, name, limit)
    }

    pub fn claims(&self, name: Option<&str>) -> Result<Vec<claim::Claim>> {
        match name {
            Some(n) => Ok(claim::get(&self.conn, n)?.into_iter().collect()),
            None => claim::list(&self.conn),
        }
    }

    pub fn channels(&self) -> Result<Vec<channel::Channel>> {
        channel::channels(&self.conn)
    }

    pub fn channel_unread(&self, pen: &str) -> Result<Vec<channel::Unread>> {
        channel::unread(&self.conn, pen)
    }

    pub fn channel_join(&self, pen: &str, channel: &str) -> Result<()> {
        channel::join(&self.conn, pen, channel)
    }

    pub fn channel_leave(&self, pen: &str, channel: &str) -> Result<()> {
        channel::leave(&self.conn, pen, channel)
    }

    pub fn channel_send(
        &self,
        pen: &str,
        channel: &str,
        body: &str,
        reply_to: Option<i64>,
    ) -> Result<channel::Message> {
        channel::send(&self.conn, pen, channel, body, reply_to)
    }

    pub fn channel_present(&self, pen: &str, boundary: Option<&str>) -> Result<usize> {
        channel::present(&self.conn, pen, boundary)
    }

    pub fn channel_invite(&self, actor: &str, pen: &str, channel: &str) -> Result<()> {
        channel::invite(&self.conn, actor, pen, channel)
    }

    pub fn channel_fetch(
        &self,
        pen: &str,
        channel: Option<&str>,
        limit: i64,
    ) -> Result<Vec<channel::Message>> {
        channel::fetch(&self.conn, pen, channel, limit)
    }

    pub fn channel_create(
        &self,
        pen: &str,
        key: &str,
        title: &str,
        members: &str,
        gate: &str,
    ) -> Result<channel::Channel> {
        channel::create(&self.conn, pen, key, title, members, gate)
    }

    pub fn channel_set_gate(
        &self,
        pen: &str,
        channel: &str,
        gate: &str,
    ) -> Result<channel::Channel> {
        channel::set_gate(&self.conn, pen, channel, gate)
    }

    pub fn channel_set_retention(
        &self,
        pen: &str,
        channel: &str,
        keep_secs: i64,
    ) -> Result<channel::Channel> {
        channel::set_retention(&self.conn, pen, channel, keep_secs)
    }

    pub fn channel_prune(&self, only: Option<&str>) -> Result<Vec<channel::Pruned>> {
        channel::prune(&self.conn, only, None)
    }

    pub fn channel_events(&self, channel: &str) -> Result<Vec<channel::Event>> {
        channel::events(&self.conn, channel)
    }

    pub fn channel_members(&self, channel: &str) -> Result<Vec<channel::Member>> {
        channel::members(&self.conn, channel)
    }

    pub fn channel_head(&self, pen: &str) -> Result<i64> {
        channel::head(&self.conn, pen)
    }

    pub fn channel_since(
        &self,
        pen: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<channel::Message>> {
        channel::since(&self.conn, pen, after_seq, limit)
    }

    pub fn channel_history(
        &self,
        channel: &str,
        since: i64,
        limit: i64,
    ) -> Result<Vec<channel::Message>> {
        channel::history(&self.conn, channel, since, limit)
    }

    pub fn channel_gate(&self, pen: &str, class: channel::Class) -> Result<()> {
        channel::gate(&self.conn, pen, class)
    }

    pub fn channel_breach(&self, pen: &str) -> Result<Option<channel::Breach>> {
        channel::breach(&self.conn, pen)
    }

    pub fn config_get(&self, key: &str) -> Result<Option<config::Item>> {
        config::get(&self.conn, &self.reader, key)
    }

    pub fn config_list(&self, prefix: Option<&str>) -> Result<Vec<config::Item>> {
        config::list(&self.conn, &self.reader, prefix)
    }

    pub fn config_set(
        &self,
        who: &str,
        key: &str,
        value: &str,
        source: Option<&str>,
    ) -> Result<config::Item> {
        let reader = store::audience::Reader::pen(who)
            .in_tree(&self.conn)
            .with_borrowed(&self.conn);
        config::set(&self.conn, &reader, who, key, value, source)
    }

    pub fn config_unset(&self, who: &str, key: &str) -> Result<Option<config::Item>> {
        let reader = store::audience::Reader::pen(who)
            .in_tree(&self.conn)
            .with_borrowed(&self.conn);
        config::unset(&self.conn, &reader, who, key)
    }

    pub fn config_define(
        &self,
        who: &str,
        key: &str,
        value: &str,
        set_audience: &str,
        read_audience: &str,
        source: &str,
    ) -> Result<config::Item> {
        let reader = store::audience::Reader::pen(who)
            .in_tree(&self.conn)
            .with_borrowed(&self.conn);
        config::define(
            &self.conn,
            &reader,
            Some(who),
            key,
            value,
            set_audience,
            read_audience,
            source,
        )
    }

    pub fn mail_send(
        &self,
        proven: &str,
        sender: &str,
        recipient: &str,
        subject: &str,
        body: &str,
        encoding: &str,
    ) -> Result<mail::Envelope> {
        mail::send(
            &self.conn, proven, sender, recipient, subject, body, encoding,
        )
    }

    pub fn mail_grant(
        &self,
        grantor: &str,
        surrogate: &str,
        scopes: &str,
        expires_at: Option<&str>,
        note: Option<&str>,
    ) -> Result<mail::Surrogate> {
        mail::grant(&self.conn, grantor, surrogate, scopes, expires_at, note)
    }

    pub fn mail_revoke(&self, grantor: &str, surrogate: &str) -> Result<usize> {
        mail::revoke(&self.conn, grantor, surrogate)
    }

    pub fn mail_may_open_box(
        &self,
        reader: &str,
        requested: &str,
        scope: mail::BoxScope,
    ) -> Result<()> {
        mail::may_open_box(&self.conn, reader, requested, scope)
    }

    pub fn mail_may_read(&self, reader: &str, requested: &str) -> Result<bool> {
        mail::may_read_box(&self.conn, reader, requested)
    }

    pub fn mail_may_ack(&self, reader: &str, requested: &str) -> Result<bool> {
        mail::may_ack_box(&self.conn, reader, requested)
    }

    pub fn mail_surrogates_of(&self, surrogate: &str) -> Result<Vec<mail::Surrogate>> {
        mail::surrogates_of(&self.conn, surrogate)
    }

    pub fn mail_inbox(
        &self,
        recipient: &str,
        since: i64,
        limit: usize,
    ) -> Result<Vec<mail::Envelope>> {
        mail::inbox(&self.conn, recipient, since, limit)
    }

    pub fn mail_head(&self, recipient: &str) -> Result<i64> {
        mail::head(&self.conn, recipient)
    }

    pub fn mail_read(&self, seq: i64) -> Result<Option<mail::Letter>> {
        mail::read(&self.conn, seq)
    }

    pub fn mail_prune(&self, keep_days: i64) -> Result<usize> {
        mail::prune(&self.conn, keep_days)
    }

    pub fn mail_cursor(&self, reader: &str, recipient: &str) -> Result<Option<mail::Cursor>> {
        mail::cursor(&self.conn, reader, recipient)
    }

    pub fn mail_advance(&self, reader: &str, recipient: &str, seq: i64) -> Result<mail::Cursor> {
        mail::advance(&self.conn, reader, recipient, seq)
    }
}

#[derive(Debug, Clone)]
pub struct MissFacts {
    pub store: String,
    pub memories: i64,
    pub schema: i32,
    pub versions: i64,
    pub newest: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct Slow;

    impl Embedder for Slow {
        fn dim(&self) -> usize {
            2
        }
        fn model_id(&self) -> &str {
            "slow-test"
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            std::thread::sleep(Duration::from_millis(200));
            Ok(vec![vec![0.0, 1.0]; texts.len()])
        }
    }

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
