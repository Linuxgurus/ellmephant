//! Projects: the **structures** half of the store (SPEC §7.1).
//!
//! A project is not a memory. Memories are FACTS — voted, ranked, judged for coherence, searched
//! by meaning. Projects are STRUCTURES — a key, a status, prose, and two relations. None of the
//! fact-machinery applies, so they live in their own tables and their keys carry the `prj-`
//! prefix, which is what lets a key cross between the two graphs (a memory body may write
//! `[[prj-x]]`) without either graph having to know about the other.
//!
//! Two relations, opposite directions, different meanings:
//!
//! - [`preload`] — project → memory, **ordered**. About *attention*: "to work in here, load
//!   these." A project preloads memories it did not author (conventions, boot notes).
//! - [`contains`] — memory → project. About *provenance*: "born here / belongs here." One home,
//!   which the schema enforces rather than merely asking for.
//!
//! Both are by-key, with no foreign keys — the same discipline as `memory_link.to_key`, so a
//! forward reference is possible and rot is *detected* ([`dangling`]) instead of raising a
//! broken-FK error.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::search::MatchMode;
use crate::search::lexical::Matcher;
use crate::store::{links, slug, tags};

/// Every project key starts with this.
pub const KEY_PREFIX: &str = "prj-";

/// The legal `status` values. `dead` is how a project ends — there is no delete.
pub const STATUSES: &[&str] = &["todo", "active", "paused", "done", "dead"];

/// The legal `owner` values.
pub const OWNERS: &[&str] = &["me", "James", "us"];

/// One project: identity, prose and shape.
///
/// `status`/`owner` are strings rather than enums because they are stored, checked and reported
/// as-is ([`STATUSES`] / [`OWNERS`] are the vocabulary); an empty value takes the default.
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
    /// `todo` | `active` | `paused` | `done` | `dead` (default `todo`).
    #[serde(default)]
    pub status: String,
    /// `me` | `James` | `us` (default `us`).
    #[serde(default)]
    pub owner: String,
    /// Aspect tags (the same comma-joined codec as `memory.tags`).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Creation timestamp.
    #[serde(default)]
    pub created: String,
    /// Last-update timestamp.
    #[serde(default)]
    pub updated: String,
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

/// Validate a `status`/`owner` value against its vocabulary, substituting `default` for an empty
/// one. `field` names the offender in the error.
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

/// Upsert a project by key in one transaction. `preload` (`Some`) replaces the ordered preload
/// wholesale; `None` leaves it alone.
///
/// Returns the resolved key. `goal`/`state`/`tags`/`status`/`owner` are written as given, so a
/// re-upsert is whole-row — read the project first if you mean to edit one field.
pub fn upsert(conn: &Connection, p: &Project, preload: Option<&[String]>) -> Result<String> {
    let key = resolve_key(conn, p)?;
    let status = choice(&p.status, STATUSES, "todo", "status")?;
    let owner = choice(&p.owner, OWNERS, "us", "owner")?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO project (key, title, goal, state, status, owner, tags, created, updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                 COALESCE(NULLIF(?8, ''), datetime('now')),
                 COALESCE(NULLIF(?9, ''), datetime('now')))
         ON CONFLICT(key) DO UPDATE SET
             title = excluded.title,
             goal = excluded.goal,
             state = excluded.state,
             status = excluded.status,
             owner = excluded.owner,
             tags = excluded.tags,
             updated = excluded.updated",
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
        ],
    )?;
    if let Some(keys) = preload {
        write_preload(&tx, &key, keys)?;
    }
    tx.commit()?;
    Ok(key)
}

/// Replace `project_key`'s ordered preload. Duplicates collapse to their first position; blank
/// keys are dropped. A key that resolves to no memory is *allowed* (a forward reference) and
/// surfaces in [`dangling`].
fn write_preload(tx: &Connection, project_key: &str, keys: &[String]) -> Result<()> {
    tx.execute(
        "DELETE FROM project_preload WHERE project_key = ?1",
        params![project_key],
    )?;
    let mut seen = HashSet::new();
    let mut position = 0i64;
    for key in keys {
        let memory_key = slug::slugify(key);
        if memory_key.is_empty() || !seen.insert(memory_key.clone()) {
            continue;
        }
        tx.execute(
            "INSERT INTO project_preload (project_key, memory_key, position) VALUES (?1, ?2, ?3)",
            params![project_key, memory_key, position],
        )?;
        position += 1;
    }
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

/// The canonical column list shared by every project read.
const PROJECT_COLUMNS: &str = "key, title, goal, state, status, owner, tags, created, updated";

/// Map one `project` row (the canonical column list) to a [`Project`].
fn row_to_project(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let tags: String = r.get(6)?;
    Ok(Project {
        key: r.get(0)?,
        title: r.get(1)?,
        goal: r.get(2)?,
        state: r.get(3)?,
        status: r.get(4)?,
        owner: r.get(5)?,
        tags: tags::decode(&tags),
        created: r.get(7)?,
        updated: r.get(8)?,
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
pub fn preload(conn: &Connection, key: &str) -> Result<Vec<Ref>> {
    refs(
        conn,
        "SELECT p.memory_key, m.title FROM project_preload p
           LEFT JOIN memory m ON m.key = p.memory_key
          WHERE p.project_key = ?1 ORDER BY p.position",
        key,
    )
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
/// project is never deleted (`status = dead` instead) — so the only rot here is a dangling memory
/// key: a forward reference the audit surfaces, exactly as SPEC §8 does for `[[…]]`.
pub fn dangling(conn: &Connection) -> Result<Vec<links::Dangling>> {
    let mut stmt = conn.prepare(
        "SELECT to_key, from_key FROM (
             SELECT p.memory_key AS to_key, p.project_key AS from_key
               FROM project_preload p
              WHERE p.memory_key NOT IN (SELECT key FROM memory)
             UNION
             SELECT c.memory_key, c.project_key
               FROM project_memory c
              WHERE c.memory_key NOT IN (SELECT key FROM memory)
         ) ORDER BY to_key, from_key",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    links::group_dangling(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
pub fn list_to_wire_json(rows: &[Project]) -> Result<String> {
    let rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|p| serde_json::json!([p.key, p.status, p.owner, p.tags, p.title, p.goal]))
        .collect();
    Ok(format!(
        "{{\"cols\":[\"key\",\"status\",\"owner\",\"tags\",\"title\",\"goal\"],\"projects\":{}}}",
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
        assert_eq!(choice("", OWNERS, "us", "owner").unwrap(), "us");
        let err = choice("nope", STATUSES, "todo", "status").unwrap_err();
        assert!(format!("{err}").contains("unknown status"), "{err}");
    }
}
