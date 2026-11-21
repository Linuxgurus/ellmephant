pub mod append;
pub mod audience;
pub mod audit;
pub mod channel;
pub mod claim;
pub mod codec;
pub mod config;
pub mod credential;
pub mod dupes;
pub mod encode;
pub mod health;
pub mod links;
pub mod mail;
pub mod maintain;
pub mod project;
pub mod relation;
mod rules;
pub mod session;
pub mod shape;
pub mod slug;
pub mod snapshot;
pub mod stats;
pub mod sudo;
pub mod tags;
pub mod version;

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::index;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Memory {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<i64>,
    pub key: String,
    pub title: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub asserted_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub edited_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub origin: Option<String>,
    #[serde(
        default = "audience::default_audience",
        skip_serializing_if = "is_public"
    )]
    pub audience: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub visits: i64,
    #[serde(default)]
    pub times_useful: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub version: Option<i64>,
}

fn is_public(a: &str) -> bool {
    a == audience::PUBLIC
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub key: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

fn taken_variants(conn: &Connection, base: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT key FROM memory WHERE key LIKE ?1")?;
    let rows = stmt.query_map(params![format!("{base}%")], |r| r.get::<_, String>(0))?;
    let mut set = HashSet::new();
    for row in rows {
        set.insert(row?);
    }
    Ok(set)
}

fn resolve_key(conn: &Connection, mem: &Memory) -> Result<String> {
    if !mem.key.trim().is_empty() {
        return Ok(slug::slugify(&mem.key));
    }
    let base = slug::slugify(&mem.title);
    Ok(slug::unique_slug(&base, &taken_variants(conn, &base)?))
}

pub(crate) fn write_links(conn: &Connection, id: i64, key: &str, body: &str) -> Result<()> {
    conn.execute("DELETE FROM memory_link WHERE from_id = ?1", params![id])?;
    for (kind, target) in links::extract_links(body) {
        if kind == links::Kind::Memory && target == key {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO memory_link (from_id, to_key, to_type, rel)
             VALUES (?1, ?2, ?3, 'see')",
            params![id, target, kind.as_str()],
        )?;
    }
    Ok(())
}

pub fn add(conn: &Connection, mem: &Memory) -> Result<(i64, String)> {
    let (id, key, _, _) = add_impl(conn, mem, None, None, None, None)?;
    Ok((id, key))
}

pub const MAX_BODY_CHARS: usize = 16_384;

fn check_body(body: &str) -> Result<()> {
    let len = body.chars().count();
    if len > MAX_BODY_CHARS {
        return Err(Error::TooLarge(format!(
            "body too long: {len} chars (limit {MAX_BODY_CHARS}); a memory is one topic, not a \
             payload — split it across memories and link them"
        )));
    }
    Ok(())
}

fn held_authority(conn: &Connection, caller: &str) -> Result<Vec<i64>> {
    Ok(sudo::reachable(conn, caller)?
        .into_iter()
        .map(|(id, _, _)| id)
        .collect())
}

pub(crate) fn may_read_audience(conn: &Connection, caller: &str, audience: &str) -> Result<bool> {
    let reader = crate::store::audience::Reader::pen(caller)
        .in_tree(conn)
        .with_borrowed(conn);
    Ok(reader.sees(audience))
}

fn answers_for(conn: &Connection, key: &str, caller: Option<&str>) -> Result<bool> {
    let Some(caller) = caller else {
        return Ok(false);
    };
    let owner: Option<Option<i64>> = conn
        .query_row(
            "SELECT owner FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?;
    let Some(Some(owner_id)) = owner else {
        return Ok(false);
    };
    let Some(caller_exact) = exact_pen_id(conn, caller)? else {
        return Ok(false);
    };
    Ok(caller_exact == owner_id || owner_pen_id(conn, Some(caller))? == Some(owner_id))
}

pub(crate) fn may_write(conn: &Connection, key: &str, caller: Option<&str>) -> Result<()> {
    let row: Option<(Option<i64>, String)> = conn
        .query_row(
            "SELECT owner, audience FROM memory WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((owner, audience)) = row else {
        return Ok(());
    };
    let Some(caller) = caller else {
        return if owner.is_none() {
            Ok(())
        } else {
            Err(Error::Invalid(format!(
                "{key} has an owner; a write must name the pen doing it"
            )))
        };
    };
    let Some(owner_id) = owner else {
        return if may_read_audience(conn, caller, &audience)? {
            Ok(())
        } else {
            Err(Error::Forbidden(format!(
                "{caller} may not write {key}: its audience does not include you"
            )))
        };
    };
    let caller_exact = exact_pen_id(conn, caller)?
        .ok_or_else(|| Error::Invalid(format!("no pen named {caller}")))?;

    if caller_exact == owner_id || owner_pen_id(conn, Some(caller))? == Some(owner_id) {
        return Ok(());
    }
    let visible = may_read_audience(conn, caller, &audience)?;
    if !visible {
        return Err(Error::Forbidden(format!(
            "{caller} may not write {key}: it exists, its audience does not include you, and you do not own it"
        )));
    }
    if is_ancestor(conn, caller_exact, owner_id)? {
        return Ok(());
    }
    for held in held_authority(conn, caller)? {
        if held == owner_id || is_ancestor(conn, held, owner_id)? {
            return Ok(());
        }
    }

    let owner_name: String = conn.query_row(
        "SELECT name FROM pen WHERE id = ?1",
        params![owner_id],
        |r| r.get(0),
    )?;
    Err(Error::Forbidden(format!(
        "{caller} may not write {key}: it is owned by {owner_name}, and only that pen or one of its ancestors may"
    )))
}

fn add_impl(
    conn: &Connection,
    mem: &Memory,
    note: Option<&str>,
    expected: Option<i64>,
    actor: Option<&str>,
    credited: Option<&str>,
) -> Result<(i64, String, bool, Option<i64>)> {
    check_body(&mem.body)?;
    let prepared = prepare(conn, mem, note)?;
    let queued = prepared.changes_document(mem);
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    may_write(&tx, &prepared.key, actor)?;
    if let Some(want) = expected {
        let current = live_version(&tx, &prepared.key)?;
        if current != want {
            return Ok((0, prepared.key, false, Some(current)));
        }
    }
    match (credited, actor) {
        (Some(credited), Some(proven)) if !credited.eq_ignore_ascii_case(proven) => {
            may_act_as(&tx, proven, credited)?;
        }
        _ => {}
    }
    let addressed = !mem.audience.trim().is_empty() && mem.audience != audience::PUBLIC;
    if addressed {
        audience::validated(&tx, &mem.audience)?;
    }
    let stored_audience: Option<String> = tx
        .query_row(
            "SELECT audience FROM memory WHERE key = ?1",
            params![prepared.key],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let stored_author: Option<String> = tx
        .query_row(
            "SELECT asserted_by FROM memory WHERE key = ?1",
            params![prepared.key],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let is_anonymous =
        |a: &str| a.trim().is_empty() || a.eq_ignore_ascii_case(crate::identity::ANONYMOUS);
    let legacy_unattributed = stored_author.as_deref().is_some_and(is_anonymous);
    if addressed
        && mem
            .asserted_by
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .is_none()
    {
        return Err(Error::Invalid(format!(
            "`{}` is addressed to others but names no author. An unattributed row has nobody to read \
             it, and the store's own default (`anonymous`) would make it the exact state the audience \
             rule exists to prevent. Name the pen that writes it, or pass `as: <pen>` with a live lend.",
            prepared.key
        )));
    }
    let author = mem
        .asserted_by
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    if addressed && author.is_some_and(is_anonymous) && !legacy_unattributed {
        return Err(Error::Invalid(format!(
            "`{}` would be addressed to others only while its author claims to be unattributed \
             (`{}`). An unattributed row has nobody to read it: name the pen that writes it, or pass \
             `as: <pen>` with a live lend so the row is theirs.",
            prepared.key,
            mem.asserted_by.as_deref().unwrap_or("")
        )));
    }
    if let (true, Some(author)) = (addressed, author) {
        let legacy_author = is_anonymous(author) && legacy_unattributed;
        if !legacy_author && !may_read_audience(&tx, author, &mem.audience)? {
            let was_trapped = stored_audience
                .as_deref()
                .map(|a| !a.trim().is_empty() && a != audience::PUBLIC)
                .unwrap_or(false);
            let same_audience = if stored_audience.as_deref() == Some(mem.audience.as_str()) {
                true
            } else {
                match (
                    audience::parse(stored_audience.as_deref().unwrap_or_default()),
                    audience::parse(&mem.audience),
                ) {
                    (Some(old), Some(new)) => {
                        let mut a: Vec<String> = new.iter().map(|t| t.token()).collect();
                        let mut b: Vec<String> = old.iter().map(|t| t.token()).collect();
                        a.sort();
                        b.sort();
                        a == b
                    }
                    _ => false,
                }
            };
            let audience_changed = !same_audience;
            return Err(Error::Invalid(format!(
                "`{}` {} invisible to its own author: the audience is `{}` and does not include \
                 `{author}`. {}\n\nAdd `pen:{author}` — or `subtree:<one of {author}'s ancestors>` if \
                 it is for a branch — or make it `public`. If this is genuinely a note FOR someone else \
                 and not for you, credit them (`as: <pen>`, which needs a live lend) so the row is \
                 theirs and the rule has an author who can read it. A row may be addressed to others; \
                 it may not be addressed to others ONLY, because then nobody who answers for it can \
                 read it.",
                prepared.key,
                if was_trapped {
                    "IS ALREADY"
                } else {
                    "would be"
                },
                mem.audience,
                if was_trapped && audience_changed {
                    "The row was already unreadable to the pen that wrote it; this write changes the \
                     audience, but the new one excludes the author too."
                } else if was_trapped {
                    "This write did not create that state — the row was already unreadable to the pen \
                     that wrote it, and this write does not change the audience."
                } else {
                    ""
                }
            )));
        }
    }
    let id = apply(&tx, mem, &prepared)?;
    tx.commit()?;
    Ok((id, prepared.key, queued, None))
}

pub fn live_version(conn: &Connection, key: &str) -> Result<i64> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory WHERE key = ?1)",
        params![key],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(0);
    }
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(n), 0) + 1 FROM memory_version WHERE type = ?1 AND key = ?2",
        params![version::MEMORY, key],
        |r| r.get(0),
    )?)
}

#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub key: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub source: Option<String>,
    pub asserted_by: Option<String>,
    pub acted_by: Option<String>,
    pub origin: Option<String>,
    pub tags: Option<Vec<String>>,
    pub private: Option<bool>,
    pub audience: Option<String>,
    pub force: bool,
    pub version_note: Option<String>,
    pub expected_version: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    pub key: String,
    pub created: bool,
    pub queued: bool,
    pub conflict: Option<i64>,
}

pub const PROTECTED: &str = "protected";

fn is_protected(tags: &[String]) -> bool {
    tags.iter().any(|t| t.eq_ignore_ascii_case(PROTECTED))
}

fn guard_protected(
    conn: &Connection,
    key: &str,
    was: &[String],
    patch: &Patch,
    writer: Option<&str>,
) -> Result<()> {
    let becomes = patch.tags.as_ref().is_some_and(|t| is_protected(t));
    if !is_protected(was) && !becomes {
        return Ok(());
    }
    let Some(claim) = crate::store::claim::get(conn, key)? else {
        return Err(Error::Invalid(format!(
            "`{key}` is PROTECTED: take the lease first (`claim_acquire name={key}`), write, then \
             release. A protected memory is writable only under a live lease."
        )));
    };
    if !claim.live {
        return Err(Error::Invalid(format!(
            "`{key}` is PROTECTED and its lease lapsed at {} (held by `{}`). Take it again.",
            claim.expires_at, claim.owner
        )));
    }
    if let Some(writer) = writer.filter(|w| !claim.owner.eq_ignore_ascii_case(w)) {
        return Err(Error::Invalid(format!(
            "`{key}` is PROTECTED and leased by `{}` until {}; `{writer}` does not hold it.",
            claim.owner, claim.expires_at
        )));
    }
    Ok(())
}

pub(crate) fn guard_protected_lease(
    conn: &Connection,
    key: &str,
    writer: Option<&str>,
) -> Result<()> {
    let Some(mem) = get(conn, key)? else {
        return Ok(());
    };
    if !is_protected(&mem.tags) {
        return Ok(());
    }
    match crate::store::claim::get(conn, key)? {
        Some(claim) if claim.live => match writer {
            Some(w) if claim.owner.eq_ignore_ascii_case(w) => Ok(()),
            Some(w) => Err(Error::Invalid(format!(
                "`{key}` is PROTECTED and leased by `{}` until {}; `{w}` does not hold it. A live lease \
                 held by someone else is not permission to delete.",
                claim.owner, claim.expires_at
            ))),
            None => Ok(()),
        },
        Some(claim) => Err(Error::Invalid(format!(
            "`{key}` is PROTECTED and its lease lapsed at {} (held by `{}`). Take it again.",
            claim.expires_at, claim.owner
        ))),
        None => Err(Error::Invalid(format!(
            "`{key}` is PROTECTED: take the lease first (`claim_acquire name={key}`), delete, then \
             release."
        ))),
    }
}

fn guard_audience(
    conn: &Connection,
    existing: Option<&Memory>,
    patch: &Patch,
    writer: Option<&str>,
) -> Result<()> {
    let Some(old) = existing else {
        return Ok(());
    };
    if old.audience == audience::PUBLIC {
        return Ok(());
    }
    if patch.private.is_none() && patch.audience.is_none() {
        return Ok(());
    }
    if answers_for(conn, &old.key, writer)? {
        return Ok(());
    }
    let writer = writer.unwrap_or_default();
    let addressed_exactly = audience::parse(&old.audience)
        .is_some_and(|targets| targets.iter().any(|t| t.token() == audience::pen(writer)));
    if addressed_exactly {
        return Ok(());
    }
    let unchanged = patch
        .audience
        .as_deref()
        .and_then(audience::parse)
        .is_some_and(|t| {
            let mut a: Vec<String> = t.iter().map(|x| x.token()).collect();
            let mut b: Vec<String> = audience::parse(&old.audience)
                .unwrap_or_default()
                .iter()
                .map(|x| x.token())
                .collect();
            a.sort();
            b.sort();
            a == b
        });
    if unchanged {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "`{}` is private to another pen: its audience is not yours to change. A row you cannot read \
         is not a row you may publish.",
        old.key
    )))
}

pub fn merge(conn: &Connection, patch: &Patch) -> Result<MergeOutcome> {
    let probe = Memory {
        key: patch.key.clone().unwrap_or_default(),
        title: patch.title.clone().unwrap_or_default(),
        ..Default::default()
    };
    let key = resolve_key(conn, &probe)?;
    let existing = get(conn, &key)?;
    let was_tags = existing
        .as_ref()
        .map(|m| m.tags.clone())
        .unwrap_or_default();
    let writer = patch.acted_by.as_deref();
    let ctx = rules::WriteCtx {
        conn,
        patch,
        key: &key,
        existing: existing.as_ref(),
        was_tags: &was_tags,
        writer,
    };
    rules::run(rules::RULES_PRE, &ctx)?;
    if existing.is_some() {
        may_write(conn, &key, writer)?;
    }
    rules::run(rules::RULES_POST, &ctx)?;

    let (mem, created) = document(&ctx)?;

    let (_id, key, queued, conflict) = add_impl(
        conn,
        &mem,
        patch.version_note.as_deref(),
        patch.expected_version,
        patch.acted_by.as_deref(),
        patch.asserted_by.as_deref(),
    )?;
    Ok(MergeOutcome {
        key,
        created: created && conflict.is_none(),
        queued,
        conflict,
    })
}

fn document(ctx: &rules::WriteCtx<'_>) -> Result<(Memory, bool)> {
    let old = ctx.existing;
    let patch = ctx.patch;
    let title = patch
        .title
        .clone()
        .or_else(|| old.map(|o| o.title.clone()))
        .unwrap_or_default();
    let body = patch
        .body
        .clone()
        .or_else(|| old.map(|o| o.body.clone()))
        .unwrap_or_default();
    let tags = patch
        .tags
        .clone()
        .or_else(|| old.map(|o| o.tags.clone()))
        .unwrap_or_default();
    let source = match &patch.source {
        Some(s) => (!s.trim().is_empty()).then(|| s.clone()),
        None => old.and_then(|o| o.source.clone()),
    };
    let origin = match (&patch.origin, old) {
        (None, old) => old.and_then(|o| o.origin.clone()),
        (Some(s), None) => (!s.trim().is_empty()).then(|| s.clone()),
        (Some(s), Some(_)) => Some(s.clone()),
    };
    let asserted_by = match old {
        Some(o) => o.asserted_by.clone(),
        None => patch.asserted_by.clone(),
    };
    let edited_by = patch.acted_by.clone().or_else(|| match old {
        Some(o) => o.edited_by.clone(),
        None => patch.asserted_by.clone(),
    });
    let audience = match &patch.audience {
        Some(spec) => audience::validated(ctx.conn, spec)?,
        None => match (patch.private, old) {
            (None, Some(o)) => o.audience.clone(),
            (private, _) => audience::for_write(
                private == Some(true),
                patch
                    .acted_by
                    .as_deref()
                    .or(patch.asserted_by.as_deref())
                    .or(old.and_then(|o| o.asserted_by.as_deref())),
            )?,
        },
    };
    Ok((
        Memory {
            key: ctx.key.to_string(),
            title,
            body,
            tags,
            source,
            asserted_by,
            edited_by,
            origin,
            audience,
            ..Default::default()
        },
        old.is_none(),
    ))
}

pub(crate) struct Prepared {
    pub(crate) key: String,
    existing: Option<(i64, String, String)>,
    note: Option<String>,
}

impl Prepared {
    pub(crate) fn changes_document(&self, mem: &Memory) -> bool {
        match &self.existing {
            None => true,
            Some((_, title, body)) => title != &mem.title || body != &mem.body,
        }
    }
}

pub(crate) fn prepare(conn: &Connection, mem: &Memory, note: Option<&str>) -> Result<Prepared> {
    let key = resolve_key(conn, mem)?;
    let existing: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, title, body FROM memory WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(Prepared {
        key,
        existing,
        note: note.map(str::to_string),
    })
}

pub(crate) fn apply(tx: &Connection, mem: &Memory, prepared: &Prepared) -> Result<i64> {
    let pending = prepared.changes_document(mem);
    let checked_audience = audience::validated(tx, audience::effective(&mem.audience))?;
    let id = match prepared.existing {
        Some((id, _, _)) => {
            if pending {
                version::snapshot(tx, id, &prepared.key, prepared.note.as_deref())?;
            }
            update_row(tx, id, mem, &checked_audience)?;
            id
        }
        None => insert_row(tx, &prepared.key, mem, &checked_audience)?,
    };
    write_links(tx, id, &prepared.key, &mem.body)?;
    if pending {
        encode::enqueue_memory(
            tx,
            &prepared.key,
            &mem.title,
            &mem.body,
            encode::PRIORITY_LIVE,
        )?;
    }
    Ok(id)
}

fn exact_pen_id(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| r.get(0),
        )
        .optional()?)
}

pub(crate) fn is_ancestor(conn: &Connection, ancestor: i64, of: i64) -> Result<bool> {
    let mut cur = of;
    for _ in 0..64 {
        let parent: Option<i64> = conn
            .query_row(
                "SELECT parent_id FROM pen WHERE id = ?1",
                params![cur],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match parent {
            Some(p) if p == ancestor => return Ok(true),
            Some(p) => cur = p,
            None => return Ok(false),
        }
    }
    Ok(false)
}

fn ensure_pen(conn: &Connection, name: &str) -> Result<i64> {
    if let Some(id) = owner_pen_id(conn, Some(name))? {
        return Ok(id);
    }
    conn.execute(
        "INSERT OR IGNORE INTO pen (name, kind) VALUES (?1, 'self')",
        params![name],
    )?;
    owner_pen_id(conn, Some(name))?
        .ok_or_else(|| Error::Invalid(format!("could not register pen {name}")))
}

fn owner_pen_id(tx: &Connection, name: Option<&str>) -> Result<Option<i64>> {
    let Some(name) = name else { return Ok(None) };
    Ok(tx
        .query_row(
            "SELECT CASE WHEN author.kind = 'finger' THEN COALESCE(author.parent_id, author.id)
                         ELSE author.id END
               FROM pen author WHERE lower(author.name) = lower(?1)",
            params![name],
            |r| r.get(0),
        )
        .optional()?)
}

fn log_pen_event(
    tx: &Connection,
    key: &str,
    action: &str,
    pen: Option<i64>,
    from: Option<i64>,
    to: Option<i64>,
    detail: Option<&str>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO pen_event (key, action, pen_id, from_pen, to_pen, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![key, action, pen, from, to, detail],
    )?;
    Ok(())
}

fn update_row(tx: &Connection, id: i64, mem: &Memory, audience: &str) -> Result<()> {
    tx.execute(
        "UPDATE memory
            SET title = ?2, body = ?3, tags = ?4, source = ?5,
                asserted_by = COALESCE(?7, asserted_by),
                origin = COALESCE(?8, origin),
                audience = ?9,
                edited_by = COALESCE(?10, ?7, edited_by),
                updated = COALESCE(NULLIF(?6, ''), datetime('now'))
          WHERE id = ?1",
        params![
            id,
            mem.title,
            mem.body,
            tags::encode(&mem.tags),
            mem.source,
            mem.updated,
            mem.asserted_by,
            mem.origin,
            audience,
            mem.edited_by
        ],
    )?;
    Ok(())
}

fn insert_row(tx: &Connection, key: &str, mem: &Memory, audience: &str) -> Result<i64> {
    tx.execute(
        "INSERT INTO memory (key, title, body, tags, source, created, updated, asserted_by, origin, audience,
                            edited_by, owner)
         VALUES (?1, ?2, ?3, ?4, ?5,
                 COALESCE(NULLIF(?6, ''), datetime('now')),
                 COALESCE(NULLIF(?7, ''), datetime('now')),
                 COALESCE(?8, 'anonymous'), ?9, ?10,
                 COALESCE(?11, ?8, 'anonymous'), ?12)",
        params![
            key,
            mem.title,
            mem.body,
            tags::encode(&mem.tags),
            mem.source,
            mem.created,
            mem.updated,
            mem.asserted_by,
            mem.origin,
            audience,
            mem.edited_by,
            match mem.asserted_by.as_deref() {
                Some(name) => Some(ensure_pen(tx, name)?),
                None => None,
            }
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

pub fn adopt(conn: &Connection, key: &str, pen: &str, force: bool) -> Result<()> {
    assign(conn, key, pen, pen, force)
}

pub fn assign(conn: &Connection, key: &str, actor: &str, to: &str, force: bool) -> Result<()> {
    let audience: Option<String> = conn
        .query_row(
            "SELECT audience FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?;
    let audience = audience.ok_or_else(|| Error::NotFound(key.to_string()))?;
    if !may_read_audience(conn, actor, &audience)? {
        return Err(Error::Invalid(format!(
            "{key} is private to {audience}; a pen cannot assign a row it cannot read"
        )));
    }
    let to_id = owner_pen_id(conn, Some(to))?.ok_or_else(|| {
        Error::Invalid(format!(
            "no pen named {to}; the pen registry has no such row"
        ))
    })?;
    let actor_id = exact_pen_id(conn, actor)?
        .ok_or_else(|| Error::Invalid(format!("no pen named {actor}")))?;
    let actor_owns = owner_pen_id(conn, Some(actor))?
        .ok_or_else(|| Error::Invalid(format!("no pen named {actor}")))?;
    let root = crate::store::sudo::live(conn, actor, "estate")?.is_some();
    if actor_owns != to_id && !root && !is_ancestor(conn, actor_id, to_id)? {
        return Err(Error::Invalid(format!(
            "{actor} may assign only to itself or its own subtree; assigning to {to} needs a live \
             borrow of estate"
        )));
    }
    let from: Option<i64> = conn
        .query_row(
            "SELECT owner FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if let Some(owner_id) = from {
        if owner_id == to_id {
            return Ok(());
        }
        if !root && !is_ancestor(conn, actor_id, owner_id)? {
            return Err(Error::Invalid(format!(
                "{actor} may not reassign {key}: only an ancestor of its owner may take an owned row"
            )));
        }
        if !force {
            return Err(Error::Invalid(format!(
                "{key} already has an owner; taking it needs force=true"
            )));
        }
    }
    let n = conn.execute(
        "UPDATE memory SET owner = ?2 WHERE key = ?1",
        params![key, to_id],
    )?;
    if n == 0 {
        return Err(Error::NotFound(key.to_string()));
    }
    let action = if actor_owns == to_id {
        "adopt"
    } else {
        "transfer"
    };
    log_pen_event(conn, key, action, Some(actor_id), from, Some(to_id), None)
}

pub fn disclaim(conn: &Connection, key: &str, pen: &str, publish: bool) -> Result<()> {
    let who =
        exact_pen_id(conn, pen)?.ok_or_else(|| Error::Invalid(format!("no pen named {pen}")))?;
    let owner_id: Option<i64> = conn
        .query_row(
            "SELECT owner FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let allowed = match owner_id {
        Some(id) => who == id || is_ancestor(conn, who, id)?,
        None => true,
    };
    if !allowed {
        return Err(Error::Invalid(format!(
            "{pen} may not disclaim {key}: only its owner or one of that owner's ancestors may"
        )));
    }
    let from = owner_id;
    let was_private = conn
        .query_row(
            "SELECT audience FROM memory WHERE key = ?1",
            params![key],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|a| a != audience::PUBLIC)
        .unwrap_or(false);
    if was_private && !publish {
        return Err(Error::Invalid(format!(
            "{key} is private; releasing it would publish another pen's words — pass publish=true to do that deliberately"
        )));
    }
    if was_private {
        let audience: String = conn.query_row(
            "SELECT audience FROM memory WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )?;
        if !may_read_audience(conn, pen, &audience)? {
            return Err(Error::Invalid(format!(
                "{pen} cannot publish {key}: the row is addressed in a way that does not include \
                 {pen}, so releasing it would publish words it cannot read"
            )));
        }
    }
    let n = conn.execute(
        "UPDATE memory SET owner = NULL, audience = ?2 WHERE key = ?1",
        params![key, audience::PUBLIC],
    )?;
    if n == 0 {
        return Err(Error::NotFound(key.to_string()));
    }
    log_pen_event(
        conn,
        key,
        if was_private { "publish" } else { "disclaim" },
        Some(who),
        from,
        None,
        Some(if was_private {
            "published on disclaim: the row was private and is now readable by every pen"
        } else {
            "released"
        }),
    )
}

pub fn transfer(conn: &Connection, key: &str, from: &str, to: &str) -> Result<()> {
    let from_id = owner_pen_id(conn, Some(from))?
        .ok_or_else(|| Error::Invalid(format!("no pen named {from}")))?;
    let to_id = owner_pen_id(conn, Some(to))?
        .ok_or_else(|| Error::Invalid(format!("no pen named {to}")))?;
    let n = conn.execute(
        "UPDATE memory SET owner = ?2 WHERE key = ?1",
        params![key, to_id],
    )?;
    if n == 0 {
        return Err(Error::NotFound(key.to_string()));
    }
    log_pen_event(
        conn,
        key,
        "transfer",
        Some(from_id),
        Some(from_id),
        Some(to_id),
        None,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rows {
    Inherit,
    Disclaim,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum History {
    Memorialize,
    Forget,
    Purge,
}

pub type PenEvent = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

#[derive(Debug, Clone, Serialize)]
pub struct RetirePreview {
    pub pen: String,
    pub parent: Option<String>,
    pub rows: i64,
    pub private_rows: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Whois {
    pub name: String,
    pub kind: String,
    pub parent: Option<String>,
    pub created: String,
    pub ceased: Option<String>,
    pub relation: Option<String>,
    pub owns: i64,
    pub descendants: i64,
}

type PenFacts = (i64, String, String, Option<i64>, String, Option<String>);

pub fn whois(conn: &Connection, viewer: &str, name: &str) -> Result<Whois> {
    let row: Option<PenFacts> = conn
        .query_row(
            "SELECT id, name, kind, parent_id, created, ceased_at FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((id, stored_name, kind, parent_id, created, ceased)) = row else {
        return Err(Error::Invalid(format!("no pen named {name}")));
    };
    let parent = match parent_id {
        Some(pid) => Some(conn.query_row(
            "SELECT name FROM pen WHERE id = ?1",
            params![pid],
            |r| r.get(0),
        )?),
        None => None,
    };
    let owns: i64 = conn.query_row(
        "SELECT count(*) FROM memory WHERE owner = ?1",
        params![id],
        |r| r.get(0),
    )?;
    let descendants: i64 = conn.query_row(
        "WITH RECURSIVE sub(id) AS (
             SELECT id FROM pen WHERE parent_id = ?1
             UNION
             SELECT c.id FROM pen c JOIN sub s ON c.parent_id = s.id
         )
         SELECT count(*) FROM sub",
        params![id],
        |r| r.get(0),
    )?;
    Ok(Whois {
        name: stored_name.clone(),
        kind,
        parent,
        created,
        ceased,
        relation: relation::relation(conn, viewer, &stored_name)?.map(|k| k.gloss()),
        owns,
        descendants,
    })
}

fn pen_row_by_id(conn: &Connection, id: i64) -> Result<(i64, Option<i64>, Option<String>)> {
    conn.query_row(
        "SELECT id, parent_id, ceased_at FROM pen WHERE id = ?1",
        params![id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen with id {id}")))
}

fn pen_row(conn: &Connection, name: &str) -> Result<(i64, Option<i64>, Option<String>)> {
    conn.query_row(
        "SELECT id, parent_id, ceased_at FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

pub const TREE_ENV: &str = "ELLMEPHANT_PEN_TREE";

fn pen_id_named(conn: &Connection, name: &str) -> Result<i64> {
    Ok(pen_row(conn, name)?.0)
}

pub fn ensure_name_uniqueness(conn: &Connection) -> Result<()> {
    let collisions: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT group_concat(name, ' / ') FROM pen GROUP BY lower(name) HAVING count(*) > 1",
        )?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    if collisions.is_empty() {
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS pen_name_nocase ON pen(lower(name))",
            [],
        )?;
    } else {
        tracing::warn!(
            collisions = %collisions.join("; "),
            "pen names collide regardless of case: new writes are refused, and a human must decide which row answers"
        );
    }
    Ok(())
}

pub fn set_parent(conn: &Connection, child: &str, parent: &str) -> Result<()> {
    let child_id = pen_id_named(conn, child)?;
    let (_, child_parent, child_ceased) = pen_row_by_id(conn, child_id)?;
    if child_ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{child} has been retired: a tombstone is not re-parented"
        )));
    }
    let parent_id = pen_id_named(conn, parent)?;
    let (_, _, parent_ceased) = pen_row_by_id(conn, parent_id)?;
    if parent_ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{parent} has been retired: a ceased pen cannot be a parent"
        )));
    }
    if child_id == parent_id {
        return Err(Error::Invalid(format!("{child} cannot be its own parent")));
    }
    if is_ancestor(conn, child_id, parent_id)? {
        return Err(Error::Invalid(format!(
            "{parent} is below {child}: that edge would make the tree a loop"
        )));
    }
    if child_parent == Some(parent_id) {
        return Ok(());
    }
    conn.execute(
        "UPDATE pen SET parent_id = ?2 WHERE id = ?1",
        params![child_id, parent_id],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seeded {
    Attached,
    Already,
    Disagrees(String),
}

pub fn seed_parent(conn: &Connection, child: &str, parent: &str) -> Result<Seeded> {
    let child_id = pen_id_named(conn, child)?;
    let (_, current, ceased) = pen_row_by_id(conn, child_id)?;
    if current.is_some() {
        let current_name: String = conn.query_row(
            "SELECT name FROM pen WHERE id = ?1",
            params![current],
            |r| r.get(0),
        )?;
        let agrees = pen_id_named(conn, parent)
            .map(|id| Some(id) == current)
            .unwrap_or(false);
        return Ok(if agrees {
            Seeded::Already
        } else {
            Seeded::Disagrees(current_name)
        });
    }
    if ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{child} has been retired: a tombstone is not re-parented"
        )));
    }
    set_parent(conn, child, parent)?;
    Ok(Seeded::Attached)
}

pub fn load_tree_at_boot(conn: &Connection) -> (usize, usize, usize, usize) {
    let Ok(spec) = std::env::var(TREE_ENV) else {
        return (0, 0, 0, 0);
    };
    let (mut attached, mut already, mut disagreed, mut rejected) = (0, 0, 0, 0);
    for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let Some((child, parent)) = entry.split_once('=') else {
            tracing::warn!(spec = %entry, "pen tree: expected `child=parent`");
            rejected += 1;
            continue;
        };
        match seed_parent(conn, child.trim(), parent.trim()) {
            Ok(Seeded::Attached) => attached += 1,
            Ok(Seeded::Already) => already += 1,
            Ok(Seeded::Disagrees(current)) => {
                tracing::warn!(
                    child = %child.trim(),
                    seed = %parent.trim(),
                    table = %current,
                    "pen tree: seed disagrees with the TABLE — the table wins; change it on purpose with `set_parent`"
                );
                disagreed += 1;
            }
            Err(e) => {
                tracing::warn!(spec = %entry, error = %e, "pen tree: edge rejected, the rest applied");
                rejected += 1;
            }
        }
    }
    tracing::info!(
        attached,
        already,
        disagreed,
        rejected,
        "pen tree: {attached} seeded, {already} already attached, {disagreed} refused in favour of the table, {rejected} malformed"
    );
    (attached, already, disagreed, rejected)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Minted {
    pub name: String,
    pub kind: String,
    pub parent: String,
    pub secret: String,
}

pub fn mint(conn: &Connection, caller: &str, name: &str, kind: &str) -> Result<Minted> {
    if kind != "self" && kind != "finger" {
        return Err(Error::Invalid(format!(
            "kind must be `self` or `finger`, not `{kind}`"
        )));
    }
    let (caller_id, _, ceased) = pen_row(conn, caller)?;
    if ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{caller} has been retired: a pen that has ceased cannot create pens"
        )));
    }
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Invalid("a pen needs a name".into()));
    }
    if name.eq_ignore_ascii_case(crate::identity::ANONYMOUS) {
        return Err(Error::Invalid(format!(
            "`{name}` is reserved: it means NO currently existing pen, so it can never be one"
        )));
    }
    if let Some((existing, ceased)) = conn
        .query_row(
            "SELECT name, ceased_at FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?
    {
        if ceased.is_some() {
            return Err(Error::Invalid(format!(
                "`{name}` is already a pen, spelled `{existing}`, and it has been retired: a name is \
                 spent — two lives under one name make every ledger row ambiguous"
            )));
        }
        return Err(Error::Invalid(format!(
            "`{name}` is already a pen, spelled `{existing}`: names are unique regardless of case, and the \
             spelling is preserved rather than folded"
        )));
    }
    if name.contains(',') || name.chars().any(char::is_whitespace) {
        return Err(Error::Invalid(format!(
            "{name} cannot be a pen name: audiences are `pen:<name>`, and a comma or a space cannot be \
             addressed"
        )));
    }
    if let Some((_, _, ceased_at)) = conn
        .query_row(
            "SELECT id, parent_id, ceased_at FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?
    {
        return Err(Error::Invalid(match ceased_at {
            Some(at) => format!(
                "{name} was a pen retired at {at}: a name is spent, and reusing it would make every \
                 ledger row mean two things"
            ),
            None => format!("{name} is already a pen"),
        }));
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO pen (name, kind, parent_id) VALUES (?1, ?2, ?3)",
        params![name, kind, caller_id],
    )?;
    let secret = crate::store::credential::issue(&tx, name, Some(caller))?;
    tx.commit()?;
    Ok(Minted {
        name: name.to_string(),
        kind: kind.to_string(),
        parent: caller.to_string(),
        secret,
    })
}

pub fn may_act_as(conn: &Connection, proven: &str, claimed: &str) -> Result<()> {
    if proven.eq_ignore_ascii_case(claimed) {
        return Ok(());
    }
    let (proven_id, _, ceased) = pen_row(conn, proven)?;
    if ceased.is_some() {
        return Err(Error::Invalid(format!("{proven} has been retired")));
    }
    let claimed_id = exact_pen_id(conn, claimed)?.ok_or_else(|| {
        Error::Invalid(format!(
            "{claimed} is not a pen: a write may name itself or a pen it minted"
        ))
    })?;
    let (_, _, claimed_ceased) = pen_row(conn, claimed)?;
    if claimed_ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{claimed} has been retired: a ceased pen cannot be credited for new work"
        )));
    }
    if !is_ancestor(conn, proven_id, claimed_id)? {
        return Err(Error::Invalid(format!(
            "{proven} may not write as {claimed}: a pen credits itself or its own subtree, never a \
             peer and never an ancestor"
        )));
    }
    if sudo::live(conn, claimed, proven)?.is_some() {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "{proven} may not write as {claimed} yet: that needs a LIVE lend — open one with `lend` (it \
         expires, and it can be revoked) rather than inheriting by tree shape"
    )))
}

pub fn retire_preview(conn: &Connection, pen: &str) -> Result<RetirePreview> {
    let (id, parent_id, _) = pen_row(conn, pen)?;
    let stored: String =
        conn.query_row("SELECT name FROM pen WHERE id = ?1", params![id], |r| {
            r.get(0)
        })?;
    let rows: i64 = conn.query_row(
        "SELECT count(*) FROM memory WHERE owner = ?1",
        params![id],
        |r| r.get(0),
    )?;
    let private_rows: i64 = conn.query_row(
        "SELECT count(*) FROM memory WHERE owner = ?1 AND audience <> ?2",
        params![id, audience::PUBLIC],
        |r| r.get(0),
    )?;
    let parent: Option<String> = match parent_id {
        Some(pid) => conn
            .query_row("SELECT name FROM pen WHERE id = ?1", params![pid], |r| {
                r.get(0)
            })
            .optional()?,
        None => None,
    };
    Ok(RetirePreview {
        pen: stored,
        parent,
        rows,
        private_rows,
    })
}

pub fn retire(
    conn: &Connection,
    pen: &str,
    caller: &str,
    rows: Rows,
    history: History,
) -> Result<RetirePreview> {
    let preview = retire_preview(conn, pen)?;
    let caller_id = exact_pen_id(conn, caller)?
        .ok_or_else(|| Error::Invalid(format!("no pen named {caller}")))?;
    let (id, parent_id, ceased_at) = pen_row(conn, pen)?;
    if ceased_at.is_some() {
        return Err(Error::Invalid(format!("{pen} is already retired")));
    }
    let Some(parent_id) = parent_id else {
        return Err(Error::Invalid(format!(
            "{pen} is a root pen and is not retirable: the estate's own standing changes by configuration, not by an API call"
        )));
    };
    if !(caller_id == parent_id || is_ancestor(conn, caller_id, parent_id)?) {
        return Err(Error::Invalid(format!(
            "{caller} may not retire {pen}: only an ancestor may"
        )));
    }
    let parent = match rows {
        Rows::Inherit => Some(parent_id),
        _ => None,
    };

    let keys: Vec<String> = {
        let mut stmt = conn.prepare("SELECT key FROM memory WHERE owner = ?1 ORDER BY key")?;

        stmt.query_map(params![id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    for key in &keys {
        match rows {
            Rows::Inherit => {
                let to = parent.expect("checked above");
                conn.execute(
                    "UPDATE memory SET owner = ?2 WHERE key = ?1",
                    params![key, to],
                )?;
                log_pen_event(
                    conn,
                    key,
                    "transfer",
                    Some(id),
                    Some(id),
                    Some(to),
                    Some("inherited on retirement"),
                )?;
            }
            Rows::Disclaim => {
                let was_private: bool = conn
                    .query_row(
                        "SELECT audience FROM memory WHERE key = ?1",
                        params![key],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .map(|a| a != audience::PUBLIC)
                    .unwrap_or(false);
                conn.execute(
                    "UPDATE memory SET owner = NULL, audience = ?2 WHERE key = ?1",
                    params![key, audience::PUBLIC],
                )?;
                log_pen_event(
                    conn,
                    key,
                    "disclaim",
                    Some(id),
                    Some(id),
                    None,
                    Some(if was_private {
                        "disclaimed on retirement: private dropped, a disclaimed row is public"
                    } else {
                        "disclaimed on retirement"
                    }),
                )?;
            }
            Rows::Delete => {
                log_pen_event(
                    conn,
                    key,
                    "delete",
                    Some(id),
                    Some(id),
                    None,
                    Some("deleted on retirement"),
                )?;
                delete_impl(conn, key, None, false)?;
            }
        }
        match history {
            History::Memorialize => {}
            History::Forget | History::Purge => {
                version::erase(conn, key)?;
            }
        }
    }

    conn.execute(
        "WITH RECURSIVE chain(id) AS (
             SELECT id FROM active_sudo WHERE powerful = ?1 OR weak = ?1
             UNION
             SELECT child.id FROM active_sudo child JOIN chain ON child.via = chain.id
         )
         DELETE FROM active_sudo WHERE id IN (SELECT id FROM chain)",
        params![id],
    )?;
    conn.execute(
        "DELETE FROM sudoers WHERE powerful = ?1 OR weak = ?1",
        params![id],
    )?;
    conn.execute(
        "UPDATE pen SET ceased_at = datetime('now') WHERE id = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM pen_credential WHERE pen_id = ?1", params![id])?;
    log_pen_event(
        conn,
        "",
        "retire",
        Some(id),
        Some(id),
        parent,
        Some(&format!(
            "rows: {:?} ({} rows, {} private), history: {:?}",
            rows, preview.rows, preview.private_rows, history
        )),
    )?;
    Ok(preview)
}

pub fn pen_events(conn: &Connection, key: &str) -> Result<Vec<PenEvent>> {
    let mut stmt = conn.prepare(
        "SELECT at, action,
                (SELECT name FROM pen WHERE id = from_pen),
                (SELECT name FROM pen WHERE id = to_pen),
                (SELECT name FROM pen WHERE id = pen_id)
           FROM pen_event WHERE key = ?1 ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![key], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unowned {
    pub key: String,
    pub created: String,
    pub asserted_by: String,
    pub why: String,
}

pub fn unowned(conn: &Connection, reader: &audience::Reader, limit: i64) -> Result<Vec<Unowned>> {
    let (pred, binds) = reader.sql();
    let mut stmt = conn.prepare(&format!(
        "SELECT key, created, asserted_by FROM memory
          WHERE owner IS NULL AND {pred}
          ORDER BY created DESC, key
          LIMIT ?{}",
        binds.len() + 1
    ))?;
    let mut all: Vec<String> = binds;
    all.push(limit.to_string());
    let rows = stmt.query_map(rusqlite::params_from_iter(all.iter()), |r| {
        let asserted_by: Option<String> = r.get(2)?;
        let asserted_by = asserted_by.unwrap_or_default();
        let why = if asserted_by.trim().is_empty()
            || asserted_by.eq_ignore_ascii_case(crate::identity::ANONYMOUS)
        {
            "unrecorded"
        } else {
            "disclaimed"
        };
        Ok(Unowned {
            key: r.get(0)?,
            created: r.get(1)?,
            asserted_by,
            why: why.to_string(),
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn delete(conn: &Connection, key: &str, acted_by: Option<&str>) -> Result<bool> {
    guard_protected_lease(conn, key, acted_by)?;
    delete_impl(conn, key, acted_by, true)
}

fn delete_impl(conn: &Connection, key: &str, acted_by: Option<&str>, log: bool) -> Result<bool> {
    let tx = conn.unchecked_transaction()?;
    if log {
        may_write(&tx, key, acted_by)?;
    }
    encode::forget(&tx, &crate::store::links::Kind::Memory.keyslug(key))?;
    let row: Option<(i64, String, Option<i64>)> = tx
        .query_row(
            "SELECT id, audience, owner FROM memory WHERE key = ?1",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let existed = row.is_some();
    if let Some((id, audience, owner)) = row {
        version::snapshot(&tx, id, key, None)?;
        tx.execute("DELETE FROM memory WHERE id = ?1", params![id])?;
        if log {
            let actor = match acted_by {
                Some(name) => exact_pen_id(&tx, name)?,
                None => None,
            };
            log_pen_event(&tx, key, "delete", actor, owner, None, None)?;
        }
        if audience == audience::PUBLIC || audience.is_empty() {
            index::vec::delete(&tx, id)?;
        } else {
            index::vec::purge(&tx, id)?;
        }
    }
    tx.commit()?;
    Ok(existed)
}

pub type UsefulSweep = (Vec<(String, i64)>, Vec<String>);

pub fn record_visit(conn: &Connection, key: &str) -> Result<i64> {
    bump(conn, key, "visits")
}

pub fn record_useful(conn: &Connection, key: &str) -> Result<i64> {
    bump(conn, key, "times_useful")
}

pub fn record_useful_many(conn: &Connection, keys: &[String]) -> Result<UsefulSweep> {
    let mut upvoted = Vec::new();
    let mut missing = Vec::new();
    for key in keys {
        match bump(conn, key, "times_useful") {
            Ok(total) => upvoted.push((key.clone(), total)),
            Err(Error::NotFound(_)) => missing.push(key.clone()),
            Err(e) => return Err(e),
        }
    }
    Ok((upvoted, missing))
}

fn bump(conn: &Connection, key: &str, column: &str) -> Result<i64> {
    let updated = conn.execute(
        &format!("UPDATE memory SET {column} = {column} + 1 WHERE key = ?1"),
        params![key],
    )?;
    if updated == 0 {
        return Err(Error::NotFound(key.to_string()));
    }
    let total = conn.query_row(
        &format!("SELECT {column} FROM memory WHERE key = ?1"),
        params![key],
        |r| r.get(0),
    )?;
    Ok(total)
}

pub fn get(conn: &Connection, key: &str) -> Result<Option<Memory>> {
    let id: Option<i64> = conn
        .query_row("SELECT id FROM memory WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?;
    match id {
        Some(id) => get_by_id(conn, id),
        None => Ok(None),
    }
}

const MEMORY_COLUMNS: &str = "id, key, title, body, tags, source, created, updated, \
     visits, times_useful, \
     (SELECT COALESCE(MAX(n), 0) + 1 FROM memory_version \
       WHERE type = 'memory' AND key = memory.key) AS version, \
     asserted_by, origin, audience, edited_by, \
     (SELECT name FROM pen WHERE pen.id = memory.owner) AS owner";

fn row_to_memory(r: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    let tags: String = r.get(4)?;
    Ok(Memory {
        id: Some(r.get(0)?),
        key: r.get(1)?,
        title: r.get(2)?,
        body: r.get(3)?,
        tags: tags::decode(&tags),
        source: r.get(5)?,
        created: r.get(6)?,
        updated: r.get(7)?,
        visits: r.get(8)?,
        times_useful: r.get(9)?,
        version: Some(r.get(10)?),
        asserted_by: r.get(11)?,
        origin: r.get(12)?,
        audience: r.get(13)?,
        edited_by: r.get(14)?,
        owner: r.get(15)?,
    })
}

pub fn get_by_id(conn: &Connection, id: i64) -> Result<Option<Memory>> {
    Ok(conn
        .query_row(
            &format!("SELECT {MEMORY_COLUMNS} FROM memory WHERE id = ?1"),
            params![id],
            row_to_memory,
        )
        .optional()?)
}

pub fn all(conn: &Connection) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(&format!("SELECT {MEMORY_COLUMNS} FROM memory ORDER BY key"))?;
    Ok(stmt
        .query_map([], row_to_memory)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn keys(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT key FROM memory ORDER BY key")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn summaries(conn: &Connection) -> Result<Vec<Summary>> {
    let mut stmt = conn.prepare("SELECT key, title, tags FROM memory ORDER BY key")?;
    let rows = stmt.query_map([], |r| {
        let tags: String = r.get(2)?;
        Ok(Summary {
            key: r.get(0)?,
            title: r.get(1)?,
            tags: tags::decode(&tags),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
