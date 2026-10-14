//! Projects: the **structures** half of the store (SPEC §7.1).
//!
//! A project is an activity completed over time — work with a beginning, a current state, and a
//! place to stand: a key, a status, prose, and two relations to memories. A memory is the other
//! half of the store: one body of closely related facts, matched by meaning, so it carries the
//! machinery (votes, visits, a coherence verdict, an embedding) that a project has no use for — a
//! shape is not something you match by meaning. Their keys carry the `prj-` prefix, which is what
//! lets a key cross between the two graphs (a memory body may write `[[prj-x]]`) without either
//! graph having to know about the other.
//!
//! Two relations, opposite directions, different meanings:
//!
//! - [`preload`] — project → memory, **ordered**, stored as the `mem_preload` column (`preload` is
//!   the *relation's* name; the column carries it, prefixed so a reader of `project` can see that
//!   its contents are memory keys and not tags). About *attention*: "to work in here, load
//!   these." A project preloads memories it did not author (conventions, boot notes). It is a
//!   **column** on `project`, not a tie table: order is a property of the list, and nothing here
//!   is cross-row (contrast [`apply_home`], where one home genuinely is).
//! - [`contains`] — memory → project. About *provenance*: "born here / belongs here." One home,
//!   which the schema enforces rather than merely asking for.
//!
//! Both are by-key, with no foreign keys — the same discipline as `memory_link.to_key`, so a
//! forward reference is possible and rot is *detected* ([`dangling`]) instead of raising a
//! broken-FK error.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::search::MatchMode;
use crate::search::lexical::Matcher;
use crate::store::{links, slug, tags};

/// Every project key starts with this.
pub const KEY_PREFIX: &str = "prj-";

/// The legal `status` values. `done` and `aborted` both end a project — there is no delete.
pub const STATUSES: &[&str] = &["todo", "active", "paused", "done", "aborted"];

/// The statuses that END a project: the work stopped, one way or the other. `end_date` rides on
/// these — *how* it ended is `status`'s job, the date only says *when*.
pub const ENDED_STATUSES: &[&str] = &["done", "aborted"];

/// One project: identity, prose and shape.
///
/// `status` is a string rather than an enum because it is stored, checked and reported as-is
/// ([`STATUSES`] is the vocabulary); `owner` is a *set* of names, stored comma-joined; the dates are
/// ISO strings, so they sort as they read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
    /// Global-unique slug, always `prj-…`; identity and link target.
    pub key: String,
    /// Smart-caveman one-liner.
    pub title: String,
    /// What this is for.
    #[serde(default)]
    pub goal: String,
    /// Mutable current-state prose: where it stands, what's next.
    #[serde(default)]
    pub state: String,
    /// `todo` | `active` | `paused` | `done` | `aborted` (default `todo`).
    #[serde(default)]
    pub status: String,
    /// Who owns it, by name — a **set**, stored comma-joined, sorted. Free text: there is no
    /// vocabulary here, because the store does not know who exists (an empty set is unassigned).
    #[serde(default)]
    pub owner: Vec<String>,
    /// Aspect tags (the same comma-joined codec as `memory.tags`).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Creation timestamp — when the *row* appeared.
    #[serde(default)]
    pub created: String,
    /// Last-update timestamp — last *write*, which is not the same as last *work*.
    #[serde(default)]
    pub updated: String,
    /// When the work started, ISO date. Stamped once on first write and correctable afterwards:
    /// [`created`] is when the row appeared, which is not the same thing — a project written down
    /// three weeks late started three weeks ago.
    #[serde(default)]
    pub start_date: String,
    /// When it ended, ISO date — empty while the project runs. Stamped when `status` becomes one of
    /// [`ENDED_STATUSES`], cleared when it is reopened, and never moved once set (an unrelated later
    /// write does not touch it). *How* it ended is `status`; this only says *when*.
    #[serde(default)]
    pub end_date: String,
}

/// A key this project points at, with its title when the target still resolves.
///
/// `title: None` means the target row does not exist (a forward reference or rot) — the read
/// reports it rather than hiding it, and [`dangling`] is the audit.
#[derive(Debug, Clone, Serialize)]
pub struct Ref {
    /// The referenced key.
    pub key: String,
    /// The target's salient line, or `None` when nothing resolves.
    pub title: Option<String>,
}

/// A project with both its relations resolved — "stand inside it".
#[derive(Debug, Clone, Serialize)]
pub struct View {
    /// The project itself.
    pub project: Project,
    /// Ordered preload (`project → memory`), titles hydrated where they resolve.
    pub preload: Vec<Ref>,
    /// Memories that call this their home (`memory → project`), key order.
    pub contains: Vec<Ref>,
}

/// Which project a memory belongs to. Defaults to [`Home::Keep`], so an upsert that says nothing
/// about provenance never silently orphans a memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Home {
    /// Leave the current home alone.
    #[default]
    Keep,
    /// Detach: this memory has no home.
    Clear,
    /// Belong to this project (normalised to `prj-…`). An error if another project already owns it.
    Set(String),
}

/// A `project_list` filter: structured predicates plus the lexical leg, all intersecting.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Only this `status`.
    pub status: Option<String>,
    /// Every listed tag must be present (AND).
    pub tags: Vec<String>,
    /// Literal/regex pattern over `key`/`title`/`goal`/`state`; `None` = no lexical leg.
    pub pattern: Option<String>,
    /// How `pattern` is read.
    pub mode: MatchMode,
}

/// The canonical form of a project key: slugified, `prj-` prefixed. One helper, used by every
/// entry point, so `"chronos"`, `"Chronos"` and `"prj-chronos"` all mean the same project.
pub fn normalize_key(raw: &str) -> String {
    let slug = slug::slugify(raw);
    if slug.starts_with(KEY_PREFIX) {
        slug
    } else {
        format!("{KEY_PREFIX}{slug}")
    }
}

/// Whether a (already normalised) key is a project key.
pub fn is_project_key(key: &str) -> bool {
    key.starts_with(KEY_PREFIX)
}

/// Resolve a write's key: an explicit key is normalised; otherwise the title is slugified under
/// the `prj-` prefix and a `-2`/`-3` suffix is added on collision.
fn resolve_key(conn: &Connection, p: &Project) -> Result<String> {
    if p.key.trim().is_empty() {
        let base = normalize_key(&p.title);
        let taken = taken_keys(conn, &base)?;
        return Ok(slug::unique_slug(&base, &taken));
    }
    let key = normalize_key(&p.key);
    if key == KEY_PREFIX {
        return Err(Error::Project("empty project key".into()));
    }
    Ok(key)
}

/// Project keys sharing `base` as a prefix (collision avoidance). `base` is `[a-z0-9-]`, so its
/// `LIKE` pattern carries no wildcards.
fn taken_keys(conn: &Connection, base: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT key FROM project WHERE key LIKE ?1")?;
    let rows = stmt.query_map(params![format!("{base}%")], |r| r.get::<_, String>(0))?;
    let mut set = HashSet::new();
    for row in rows {
        set.insert(row?);
    }
    Ok(set)
}

/// Validate a single vocabulary value (a `status`) against its `allowed` set, substituting
/// `default` for an empty one. `field` names the offender in the error.
fn choice(value: &str, allowed: &[&str], default: &str, field: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(default.to_string());
    }
    if allowed.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(Error::Project(format!(
            "unknown {field} `{value}` (one of: {})",
            allowed.join(", ")
        )))
    }
}

/// True for `YYYY-MM-DD` and `YYYY-MM-DD HH:MM:SS` — the two shapes `datetime('now')` writes, and
/// the shapes that sort as they read, which is the whole reason to insist on them. Shape only: a
/// shaped-but-wrong date (`2026-13-45`) is the writer's lie, not the store's, and the alternative —
/// free text — breaks every "what ended in September" query silently.
fn is_iso_date(value: &str) -> bool {
    let b = value.as_bytes();
    let digits = |ix: &[usize]| ix.iter().all(|&i| b[i].is_ascii_digit());
    match b.len() {
        10 => b[4] == b'-' && b[7] == b'-' && digits(&[0, 1, 2, 3, 5, 6, 8, 9]),
        19 => {
            b[4] == b'-'
                && b[7] == b'-'
                && b[10] == b' '
                && b[13] == b':'
                && b[16] == b':'
                && digits(&[0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18])
        }
        _ => false,
    }
}

/// Validate a `start_date`/`end_date` value: empty, or an ISO date ([`is_iso_date`]). `field` names
/// the offender in the error.
fn date_field(value: &str, field: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || is_iso_date(value) {
        Ok(value.to_string())
    } else {
        Err(Error::Project(format!(
            "`{field}` must be an ISO date (`YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS`), not `{value}`"
        )))
    }
}

/// Upsert a project by key in one transaction. `preload` (`Some`) replaces the ordered preload
/// wholesale; `None` leaves it alone.
///
/// Returns the resolved key. `goal`/`state`/`tags`/`status`/`owner` are written as given, so a
/// re-upsert is whole-row — read the project first if you mean to edit one field. The dates are the
/// exception, and deliberately so: `start_date` is stamped once and left alone afterwards, and
/// `end_date` follows `status` rather than the caller's silence. `created`/`updated` are the row's
/// own timestamps and never move backwards.
pub fn upsert(conn: &Connection, p: &Project, preload: Option<&[String]>) -> Result<String> {
    let key = resolve_key(conn, p)?;
    let status = choice(&p.status, STATUSES, "todo", "status")?;
    let owner = encode_owners(&p.owner);
    let start_date = date_field(&p.start_date, "start_date")?;
    let end_date = date_field(&p.end_date, "end_date")?;
    let ended = ENDED_STATUSES.contains(&status.as_str());
    if !end_date.is_empty() && !ended {
        return Err(Error::Project(format!(
            "`end_date` on a project that has not ended (`status` is `{status}`): set `status` to {} \
             to end it, or leave the date out",
            ENDED_STATUSES.join(" or ")
        )));
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO project (key, title, goal, state, status, owner, tags, created, updated,
                              start_date, end_date)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                 COALESCE(NULLIF(?8, ''), datetime('now')),
                 COALESCE(NULLIF(?9, ''), datetime('now')),
                 COALESCE(NULLIF(?10, ''), datetime('now')),
                 CASE WHEN ?12 THEN COALESCE(NULLIF(?11, ''), datetime('now')) ELSE '' END)
         ON CONFLICT(key) DO UPDATE SET
             title = excluded.title,
             goal = excluded.goal,
             state = excluded.state,
             status = excluded.status,
             owner = excluded.owner,
             tags = excluded.tags,
             updated = excluded.updated,
             start_date = CASE WHEN ?10 <> '' THEN ?10 ELSE project.start_date END,
             end_date = CASE WHEN ?12
                             THEN COALESCE(NULLIF(?11, ''), NULLIF(project.end_date, ''),
                                           datetime('now'))
                             ELSE '' END",
        params![
            key,
            p.title,
            p.goal,
            p.state,
            status,
            owner,
            tags::encode(&p.tags),
            p.created,
            p.updated,
            start_date,
            end_date,
            ended,
        ],
    )?;
    if let Some(keys) = preload {
        write_preload(&tx, &key, keys)?;
    }
    tx.commit()?;
    Ok(key)
}

/// Encode an ordered preload as the `project.mem_preload` column: keys slugified, blanks dropped and
/// duplicates collapsed to their first position. The ORDER is the payload, so — unlike
/// [`tags::encode`] — nothing is sorted. A key is `[a-z0-9-]` (see [`slug::slugify`]), so a comma
/// can never occur inside one and the delimiter needs no escaping.
fn encode_preload(keys: &[String]) -> String {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for key in keys {
        // A blank entry means nothing to load. Checked *before* slugifying: `slug::slugify` never
        // returns empty (it falls back to `untitled`), so a blank would otherwise become a key.
        if key.trim().is_empty() {
            continue;
        }
        let memory_key = slug::slugify(key);
        if seen.insert(memory_key.clone()) {
            out.push(memory_key);
        }
    }
    out.join(",")
}

/// Decode `project.mem_preload` back into its ordered keys. Inverse of [`encode_preload`], minus the
/// normalisation that already happened on the way in.
fn decode_preload(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .collect()
}

/// Encode an owner set as the `project.owner` column: blanks dropped, duplicates collapsed
/// **case-insensitively** (so `James` and `james` are one person — the first spelling wins, because
/// these are proper names and lowercasing them would be a lie), then **sorted** case-insensitively,
/// since owners are a set with no order and the stored form must not depend on the order they were
/// typed in. Contrast [`encode_preload`], where the order *is* the payload.
///
/// Free text, deliberately: no vocabulary, no `me`/`us`, nothing to validate against. The store does
/// not know who exists, and it cannot — `me` resolves differently for every reader, which is
/// precisely what a shared store cannot afford. An empty set is an unassigned project.
pub fn encode_owners(owners: &[String]) -> String {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for owner in owners {
        let name = owner.trim();
        if name.is_empty() || !seen.insert(name.to_lowercase()) {
            continue;
        }
        out.push(name.to_string());
    }
    out.sort_by_key(|name| name.to_lowercase());
    out.join(",")
}

/// Decode `project.owner` back into its set of names — the inverse of [`encode_owners`], minus the
/// normalisation that already happened on the way in.
fn decode_owners(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Replace `project_key`'s ordered preload (one column, one write). Duplicates collapse to their
/// first position; blank keys are dropped. A key that resolves to no memory is *allowed* (a
/// forward reference) and surfaces in [`dangling`].
fn write_preload(tx: &Connection, project_key: &str, keys: &[String]) -> Result<()> {
    tx.execute(
        "UPDATE project SET mem_preload = ?2 WHERE key = ?1",
        params![project_key, encode_preload(keys)],
    )?;
    Ok(())
}

/// Apply a [`Home`] verdict to `memory_key` inside the caller's transaction (the memory write
/// path calls this, so membership lands atomically with the row).
///
/// `Home::Set` requires the project to exist — a typo in a home is a mistake, not a forward
/// reference — and refuses to move a memory that another project already owns.
pub fn apply_home(tx: &Connection, memory_key: &str, home: &Home) -> Result<()> {
    match home {
        Home::Keep => Ok(()),
        Home::Clear => {
            tx.execute(
                "DELETE FROM project_memory WHERE memory_key = ?1",
                params![memory_key],
            )?;
            Ok(())
        }
        Home::Set(project) => {
            let project = normalize_key(project);
            if !exists(tx, &project)? {
                return Err(Error::NoProject(project));
            }
            let current: Option<String> = tx
                .query_row(
                    "SELECT project_key FROM project_memory WHERE memory_key = ?1",
                    params![memory_key],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(current) = current
                && current != project
            {
                return Err(Error::Project(format!(
                    "`{memory_key}` already belongs to `{current}` \
                     (one home per memory; detach with project: \"\" first)"
                )));
            }
            tx.execute(
                "INSERT OR REPLACE INTO project_memory (memory_key, project_key) VALUES (?1, ?2)",
                params![memory_key, project],
            )?;
            Ok(())
        }
    }
}

/// Whether a project row exists.
fn exists(conn: &Connection, key: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row("SELECT 1 FROM project WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(found.is_some())
}

/// The canonical column list shared by every project read. The `mem_preload` column is deliberately
/// absent: [`preload`] is a *relation*, read by that function and never mapped onto [`Project`].
const PROJECT_COLUMNS: &str =
    "key, title, goal, state, status, owner, tags, created, updated, start_date, end_date";

/// Map one `project` row (the canonical column list) to a [`Project`].
fn row_to_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let tags: String = r.get(6)?;
    let owner: String = r.get(5)?;
    Ok(Project {
        key: r.get(0)?,
        title: r.get(1)?,
        goal: r.get(2)?,
        state: r.get(3)?,
        status: r.get(4)?,
        owner: decode_owners(&owner),
        tags: tags::decode(&tags),
        created: r.get(7)?,
        updated: r.get(8)?,
        start_date: r.get(9)?,
        end_date: r.get(10)?,
    })
}

/// Fetch one project by key (`prj-` is optional in the argument; see [`normalize_key`]).
pub fn get(conn: &Connection, key: &str) -> Result<Option<Project>> {
    let key = normalize_key(key);
    Ok(conn
        .query_row(
            &format!("SELECT {PROJECT_COLUMNS} FROM project WHERE key = ?1"),
            params![key],
            row_to_project,
        )
        .optional()?)
}

/// Run a `(key, title)` query for one project and collect [`Ref`]s in query order. The join is a
/// `LEFT JOIN`, so a key that resolves to nothing comes back with `title: None` rather than
/// vanishing — the read reports rot, it does not hide it.
fn refs(conn: &Connection, sql: &str, key: &str) -> Result<Vec<Ref>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![normalize_key(key)], |r| {
        Ok(Ref {
            key: r.get(0)?,
            title: r.get::<_, Option<String>>(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The ordered preload (`project → memory`), titles hydrated where they resolve.
///
/// The list is one column, so this is a parse plus a single lookup for the titles — which are
/// then put back into *list* order, because the query's order must never leak into the result.
pub fn preload(conn: &Connection, key: &str) -> Result<Vec<Ref>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT mem_preload FROM project WHERE key = ?1",
            params![normalize_key(key)],
            |r| r.get(0),
        )
        .optional()?;
    let keys = raw.map_or_else(Vec::new, |raw| decode_preload(&raw));
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; keys.len()].join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT key, title FROM memory WHERE key IN ({placeholders})"
    ))?;
    let titles: HashMap<String, String> = stmt
        .query_map(rusqlite::params_from_iter(keys.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(keys
        .into_iter()
        .map(|key| Ref {
            title: titles.get(&key).cloned(),
            key,
        })
        .collect())
}

/// The memories that call `key` home (`memory → project`), key order.
pub fn contains(conn: &Connection, key: &str) -> Result<Vec<Ref>> {
    refs(
        conn,
        "SELECT c.memory_key, m.title FROM project_memory c
           LEFT JOIN memory m ON m.key = c.memory_key
          WHERE c.project_key = ?1 ORDER BY c.memory_key",
        key,
    )
}

/// The project plus both relations — the `project_get` read ("stand inside it").
pub fn view(conn: &Connection, key: &str) -> Result<Option<View>> {
    let Some(project) = get(conn, key)? else {
        return Ok(None);
    };
    Ok(Some(View {
        preload: preload(conn, &project.key)?,
        contains: contains(conn, &project.key)?,
        project,
    }))
}

/// Every project matching `filter`, key order. `state` rides along; the caller decides what to
/// render (the list wire leaves it out).
///
/// The predicates intersect, like a memory search: status, then tags (AND), then the lexical leg.
/// The pattern is compiled once ([`Matcher::new`]) and matched over `key`/`title`/`goal`/`state` —
/// the same leg `memory_search` uses, over the project columns.
pub fn list(conn: &Connection, filter: &Filter) -> Result<Vec<Project>> {
    let matcher = match filter.pattern.as_deref().map(str::trim) {
        Some(pattern) if !pattern.is_empty() => Some(Matcher::new(pattern, filter.mode)?),
        _ => None,
    };
    let status = filter
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let wanted: Vec<String> = filter
        .tags
        .iter()
        .filter_map(|t| tags::normalize(t))
        .collect();

    let mut stmt = conn.prepare(&format!(
        "SELECT {PROJECT_COLUMNS} FROM project ORDER BY key"
    ))?;
    let rows = stmt.query_map([], row_to_project)?;
    let mut out = Vec::new();
    for row in rows {
        let project = row?;
        if status.is_some_and(|s| project.status != s) {
            continue;
        }
        if !wanted.iter().all(|t| project.tags.contains(t)) {
            continue;
        }
        if let Some(matcher) = &matcher {
            let fields = [&project.key, &project.title, &project.goal, &project.state];
            if !fields.into_iter().any(|f| matcher.matches(f)) {
                continue;
            }
        }
        out.push(project);
    }
    Ok(out)
}

/// Project-side rot: `preload`/`contains` keys that resolve to no memory.
///
/// Membership cannot outlive a project — [`apply_home`] requires the project to exist and a
/// project is never deleted (`status = aborted` instead) — so the only rot here is a dangling memory
/// key: a forward reference the audit surfaces, exactly as SPEC §8 does for `[[…]]`.
///
/// `contains` is a table, so SQL finds its rot directly; `preload` is a column (`mem_preload`), so
/// its list is parsed here and checked against the live memory keys. Both feed one report, so the
/// audit reads the same either way.
pub fn dangling(conn: &Connection) -> Result<Vec<links::Dangling>> {
    let mut rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT c.memory_key, c.project_key FROM project_memory c
              WHERE c.memory_key NOT IN (SELECT key FROM memory)",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let present: HashSet<String> = {
        let mut stmt = conn.prepare("SELECT key FROM memory")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<HashSet<_>>>()?
    };
    let mut stmt = conn.prepare("SELECT key, mem_preload FROM project")?;
    let projects = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in projects {
        let (project_key, raw) = row?;
        for key in decode_preload(&raw) {
            if !present.contains(&key) {
                rows.push((key, project_key.clone()));
            }
        }
    }
    // `group_dangling` folds *consecutive* rows, so the sort is load-bearing.
    rows.sort();
    links::group_dangling(rows)
}

/// How many projects exist (reported by `doctor`).
pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM project", [], |r| r.get(0))?)
}

/// How many memories have a home (reported by `doctor`).
pub fn homed(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM project_memory", [], |r| r.get(0))?)
}

/// `project_list` for the MCP wire: a `cols` header plus positional rows, one per line (the
/// convention for list-shaped reads; `state` is deliberately absent — fetch it with `project_get`).
/// The dates *are* here: "what did I finish last month" is a list question.
pub fn list_to_wire_json(rows: &[Project]) -> Result<String> {
    let rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|p| {
            serde_json::json!([
                p.key,
                p.status,
                p.owner,
                p.tags,
                p.title,
                p.goal,
                p.start_date,
                p.end_date
            ])
        })
        .collect();
    Ok(format!(
        "{{\"cols\":[\"key\",\"status\",\"owner\",\"tags\",\"title\",\"goal\",\"start_date\",\
          \"end_date\"],\"projects\":{}}}",
        crate::wire::rows_array(&rows)
    ))
}

/// `project_get` for the MCP wire: the project as a keyed object (one row, many fields) plus the
/// two ordered relations as their own `cols`/`rows` tables.
pub fn view_to_wire_json(view: &View) -> Result<String> {
    let table = |refs: &[Ref]| {
        let rows: Vec<serde_json::Value> = refs
            .iter()
            .map(|r| serde_json::json!([r.key, r.title]))
            .collect();
        crate::wire::rows_array(&rows)
    };
    Ok(format!(
        "{{\"project\":{},\"preload\":{{\"cols\":[\"key\",\"title\"],\"rows\":{}}},\
          \"contains\":{{\"cols\":[\"key\",\"title\"],\"rows\":{}}}}}",
        serde_json::to_string(&view.project)?,
        table(&view.preload),
        table(&view.contains),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_key_prefixes_and_slugs() {
        assert_eq!(normalize_key("chronos"), "prj-chronos");
        assert_eq!(normalize_key("Chronos"), "prj-chronos");
        assert_eq!(normalize_key("prj-chronos"), "prj-chronos");
        assert_eq!(normalize_key("prj sessions"), "prj-sessions");
        assert_eq!(normalize_key("  Deep Work  "), "prj-deep-work");
    }

    #[test]
    fn is_project_key_reads_the_prefix() {
        assert!(is_project_key("prj-x"));
        assert!(!is_project_key("x"));
        assert!(!is_project_key("todo-ellmephant-projects"));
    }

    #[test]
    fn choice_defaults_and_rejects() {
        assert_eq!(choice("", STATUSES, "todo", "status").unwrap(), "todo");
        assert_eq!(
            choice(" active ", STATUSES, "todo", "status").unwrap(),
            "active"
        );
        let err = choice("nope", STATUSES, "todo", "status").unwrap_err();
        assert!(format!("{err}").contains("unknown status"), "{err}");
    }

    #[test]
    fn preload_codec_keeps_order_and_drops_noise() {
        let keys = vec![
            "boot-note".to_string(),
            "  Chronos Rules ".to_string(),
            "boot-note".to_string(), // a duplicate collapses to its first position
            String::new(),           // blank: dropped, never `untitled`
            "   ".to_string(),
        ];
        // Order is the payload, so nothing is sorted the way `tags` sorts.
        assert_eq!(encode_preload(&keys), "boot-note,chronos-rules");
        assert_eq!(encode_preload(&[]), "");
        assert_eq!(decode_preload(""), Vec::<String>::new());
        assert_eq!(
            decode_preload("a,b"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn owner_codec_keeps_names_free_and_stores_a_canonical_set() {
        // Free text: there is no vocabulary to validate against, and no `me`/`us`.
        assert_eq!(
            encode_owners(&["Wile E. Coyote".to_string()]),
            "Wile E. Coyote"
        );
        // A set: sorted (case-insensitively), so the stored form never depends on typing order.
        assert_eq!(
            encode_owners(&["James".to_string(), "Cline".to_string()]),
            "Cline,James"
        );
        // Case is kept as typed -- proper names -- but is not identity: `James` and `james` are one
        // person, so the first spelling wins.
        assert_eq!(
            encode_owners(&["James".to_string(), "james".to_string()]),
            "James",
            "one person, one entry"
        );
        assert_eq!(encode_owners(&["".to_string(), "  ".to_string()]), "");
        assert_eq!(
            encode_owners(&[]),
            "",
            "no owner recorded is an honest answer"
        );
        assert_eq!(
            decode_owners("Cline,James"),
            vec!["Cline".to_string(), "James".to_string()]
        );
        assert!(decode_owners("").is_empty());
    }

    #[test]
    fn iso_dates_are_shape_checked() {
        assert!(is_iso_date("2026-10-15"));
        assert!(
            is_iso_date("2026-10-15 09:30:00"),
            "the shape datetime('now') writes"
        );
        assert!(!is_iso_date("2026-1-5"), "no zero padding, no lexical sort");
        assert!(!is_iso_date("15/10/2026"));
        assert!(
            !is_iso_date("2026-10-15T09:30:00"),
            "the stored form is the SQL one"
        );
        assert!(!is_iso_date("yesterday"));
        assert!(!is_iso_date(""));
        assert_eq!(
            date_field(" 2026-10-15 ", "start_date").unwrap(),
            "2026-10-15",
            "trimmed"
        );
        assert_eq!(
            date_field("", "start_date").unwrap(),
            "",
            "empty is 'not said'"
        );
        let err = date_field("Sept 3", "start_date").unwrap_err();
        assert!(format!("{err}").contains("ISO date"), "{err}");
    }
}
