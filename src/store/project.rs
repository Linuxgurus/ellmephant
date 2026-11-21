use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::search::MatchMode;
use crate::search::lexical::Matcher;
use crate::store::{codec, links, slug, tags};

const LEGACY_PREFIX: &str = "prj-";

fn strip_marker(raw: &str) -> &str {
    let raw = raw.trim();
    match raw
        .strip_prefix(links::PROJECT_MARKER)
        .or_else(|| raw.strip_prefix(LEGACY_PREFIX))
    {
        Some(rest) => rest.trim(),
        None => raw,
    }
}

pub fn normalize_key(raw: &str) -> String {
    slug::slugify(strip_marker(raw))
}

fn resolve_key(conn: &Connection, p: &Project) -> Result<String> {
    if p.key.trim().is_empty() {
        let base = normalize_key(&p.title);
        let taken = taken_keys(conn, &base)?;
        return Ok(slug::unique_slug(&base, &taken));
    }
    if strip_marker(&p.key).trim().is_empty() {
        return Err(Error::Project("empty project key".into()));
    }
    Ok(normalize_key(&p.key))
}

pub const STATUSES: &[&str] = &["todo", "active", "paused", "done", "aborted"];

pub const ENDED_STATUSES: &[&str] = &["done", "aborted"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub owner: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub start_date: String,
    #[serde(default)]
    pub end_date: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hydrate {
    #[default]
    None,
    Snippet,
    Full,
}

impl Hydrate {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_lowercase().as_str() {
            "" | "none" => Ok(Hydrate::None),
            "snippet" => Ok(Hydrate::Snippet),
            "full" => Ok(Hydrate::Full),
            other => Err(Error::Project(format!(
                "unknown hydrate `{other}` (one of: none, snippet, full)"
            ))),
        }
    }

    fn body(self, body: &str) -> Option<String> {
        match self {
            Hydrate::None => None,
            Hydrate::Snippet => Some(crate::search::snippet(body, crate::search::SNIPPET_CHARS)),
            Hydrate::Full => Some(body.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Ref {
    pub key: String,
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub project: Project,
    pub preload: Vec<Ref>,
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub status: Option<String>,
    pub tags: Vec<String>,
    pub owner: Vec<String>,
    pub pattern: Option<String>,
    pub mode: MatchMode,
}

fn taken_keys(conn: &Connection, base: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT key FROM project WHERE key LIKE ?1")?;
    let rows = stmt.query_map(params![format!("{base}%")], |r| r.get::<_, String>(0))?;
    let mut set = HashSet::new();
    for row in rows {
        set.insert(row?);
    }
    Ok(set)
}

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

pub fn upsert(conn: &Connection, p: &Project, preload: Option<&[String]>) -> Result<String> {
    let key = resolve_key(conn, p)?;
    let status = choice(&p.status, STATUSES, "todo", "status")?;
    let owner = encode_owners(&p.owner)?;
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
                 CASE WHEN ?10 <> '' THEN ?10
                      WHEN ?5 <> 'todo' THEN datetime('now')
                      ELSE '' END,
                 CASE WHEN ?12 THEN COALESCE(NULLIF(?11, ''), datetime('now')) ELSE '' END)
         ON CONFLICT(key) DO UPDATE SET
             title = excluded.title,
             goal = excluded.goal,
             state = excluded.state,
             status = excluded.status,
             owner = excluded.owner,
             tags = excluded.tags,
             updated = excluded.updated,
             start_date = CASE WHEN ?10 <> '' THEN ?10
                               WHEN project.start_date <> '' THEN project.start_date
                               WHEN ?5 <> 'todo' THEN datetime('now')
                               ELSE '' END,
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

fn encode_preload(keys: &[String]) -> Result<String> {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for raw in keys {
        if raw.trim().is_empty() {
            continue;
        }
        let (kind, token) = links::parse_target(raw.trim());
        if kind == links::Kind::Project {
            return Err(Error::Project(format!(
                "`preload` holds memory keys; `{raw}` names a project — write it `{}`, or forget the \
                 marker to point at a memory of that name",
                links::PROJECT_MARKER
            )));
        }
        let memory_key = slug::slugify(&token);
        if seen.insert(memory_key.clone()) {
            out.push(memory_key);
        }
    }
    Ok(out.join(","))
}

fn decode_preload(raw: &str) -> Vec<String> {
    codec::split(raw)
}

pub fn encode_owners(owners: &[String]) -> Result<String> {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for owner in owners {
        let name = owner.trim();
        if name.is_empty() {
            continue;
        }
        if name.contains(codec::DELIMITER) {
            return Err(Error::Project(format!(
                "`{name}` contains a comma; owners are stored as a comma-joined list, so a name \
                 cannot carry one. Write the name without the comma — a name is free text, so the \
                 delimiter cannot be folded away the way a tag's is"
            )));
        }
        if !seen.insert(name.to_lowercase()) {
            continue;
        }
        out.push(name.to_string());
    }
    out.sort_by_key(|name| name.to_lowercase());
    Ok(out.join(","))
}

fn decode_owners(raw: &str) -> Vec<String> {
    codec::split(raw)
}

fn write_preload(tx: &Connection, project_key: &str, keys: &[String]) -> Result<()> {
    tx.execute(
        "UPDATE project SET mem_preload = ?2 WHERE key = ?1",
        params![project_key, encode_preload(keys)?],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub key: Option<String>,
    pub title: Option<String>,
    pub goal: Option<String>,
    pub state: Option<String>,
    pub status: Option<String>,
    pub owner: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub preload: Option<Vec<String>>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    pub key: String,
    pub created: bool,
}

pub fn merge(conn: &Connection, patch: &Patch) -> Result<MergeOutcome> {
    if patch.status.as_ref().is_some_and(|s| s.trim().is_empty()) {
        return Err(Error::Project(
            "`status` cannot be empty; omit it to leave the stored status alone".into(),
        ));
    }
    if patch.title.as_ref().is_some_and(|t| t.trim().is_empty()) {
        return Err(Error::Project(
            "`title` cannot be empty; omit it to leave the stored title alone".into(),
        ));
    }

    let probe = Project {
        key: patch.key.clone().unwrap_or_default(),
        title: patch.title.clone().unwrap_or_default(),
        ..Default::default()
    };
    let key = resolve_key(conn, &probe)?;
    let existing = get(conn, &key)?;
    let created = existing.is_none();

    let merged = match &existing {
        Some(old) => Project {
            key: key.clone(),
            title: patch.title.clone().unwrap_or_else(|| old.title.clone()),
            goal: patch.goal.clone().unwrap_or_else(|| old.goal.clone()),
            state: patch.state.clone().unwrap_or_else(|| old.state.clone()),
            status: patch.status.clone().unwrap_or_else(|| old.status.clone()),
            owner: patch.owner.clone().unwrap_or_else(|| old.owner.clone()),
            tags: patch.tags.clone().unwrap_or_else(|| old.tags.clone()),
            start_date: patch
                .start_date
                .clone()
                .unwrap_or_else(|| old.start_date.clone()),
            ..Default::default()
        },
        None => {
            if patch.title.as_ref().is_none_or(|t| t.trim().is_empty()) {
                return Err(Error::Project(
                    "a new project needs a `title`; an existing key can be patched field by field"
                        .into(),
                ));
            }
            Project {
                key: key.clone(),
                title: patch.title.clone().unwrap_or_default(),
                goal: patch.goal.clone().unwrap_or_default(),
                state: patch.state.clone().unwrap_or_default(),
                status: patch.status.clone().unwrap_or_default(),
                owner: patch.owner.clone().unwrap_or_default(),
                tags: patch.tags.clone().unwrap_or_default(),
                start_date: patch.start_date.clone().unwrap_or_default(),
                ..Default::default()
            }
        }
    };

    let status = choice(&merged.status, STATUSES, "todo", "status")?;
    let ended = ENDED_STATUSES.contains(&status.as_str());
    let stated_end = patch.end_date.clone().filter(|d| !d.trim().is_empty());
    if stated_end.is_some() && !ended {
        return Err(Error::Project(format!(
            "`end_date` on a project that has not ended (`status` is `{status}`): set `status` to {} \
             to end it, or leave the date out",
            ENDED_STATUSES.join(" or ")
        )));
    }
    let end_date = if ended {
        stated_end
            .or_else(|| {
                existing
                    .as_ref()
                    .map(|old| old.end_date.clone())
                    .filter(|d| !d.is_empty())
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    let merged = Project {
        status,
        end_date,
        ..merged
    };

    let key = upsert(conn, &merged, patch.preload.as_deref())?;
    Ok(MergeOutcome { key, created })
}

const PROJECT_COLUMNS: &str =
    "key, title, goal, state, status, owner, tags, created, updated, start_date, end_date";

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

pub fn preload(conn: &Connection, key: &str, hydrate: Hydrate) -> Result<Vec<Ref>> {
    preload_seen(
        conn,
        key,
        hydrate,
        &crate::store::audience::Reader::everything(),
    )
}

pub fn preload_seen(
    conn: &Connection,
    key: &str,
    hydrate: Hydrate,
    reader: &crate::store::audience::Reader,
) -> Result<Vec<Ref>> {
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
        "SELECT key, title, body, audience FROM memory WHERE key IN ({placeholders})"
    ))?;
    let rows: HashMap<String, (String, String, String)> = stmt
        .query_map(rusqlite::params_from_iter(keys.iter()), |r| {
            Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(keys
        .into_iter()
        .filter_map(|key| match rows.get(&key) {
            Some((_, _, audience)) if !reader.sees(audience) => None,
            Some((title, body, _)) => Some(Ref {
                key,
                title: Some(title.clone()),
                body: hydrate.body(body),
            }),
            None => Some(Ref {
                key,
                title: None,
                body: None,
            }),
        })
        .collect())
}

pub fn repoint_preload(conn: &Connection, from: &str, into: &str) -> Result<usize> {
    let from = slug::slugify(from);
    let into = slug::slugify(into);
    let rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT key, mem_preload FROM project")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut changed = 0;
    for (key, raw) in rows {
        let keys = decode_preload(&raw);
        if !keys.iter().any(|k| k == &from) {
            continue;
        }
        let mut repointed: Vec<String> = Vec::new();
        for k in keys {
            let k = if k == from { into.clone() } else { k };
            if !repointed.contains(&k) {
                repointed.push(k);
            }
        }
        conn.execute(
            "UPDATE project SET mem_preload = ?2, updated = datetime('now') WHERE key = ?1",
            params![key, encode_preload(&repointed)?],
        )?;
        changed += 1;
    }
    Ok(changed)
}

pub fn view(conn: &Connection, key: &str, hydrate: Hydrate) -> Result<Option<View>> {
    view_seen(
        conn,
        key,
        hydrate,
        &crate::store::audience::Reader::everything(),
    )
}

pub fn view_seen(
    conn: &Connection,
    key: &str,
    hydrate: Hydrate,
    reader: &crate::store::audience::Reader,
) -> Result<Option<View>> {
    let Some(project) = get(conn, key)? else {
        return Ok(None);
    };
    Ok(Some(View {
        preload: preload_seen(conn, &project.key, hydrate, reader)?,
        project,
    }))
}

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
    let owners: Vec<String> = filter
        .owner
        .iter()
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
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
        if !owners
            .iter()
            .all(|want| project.owner.iter().any(|o| o.to_lowercase() == *want))
        {
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

pub fn dangling(conn: &Connection) -> Result<Vec<links::Dangling>> {
    let mut rows: Vec<(String, links::Kind, String)> = Vec::new();
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
                rows.push((key, links::Kind::Memory, project_key.clone()));
            }
        }
    }
    rows.sort();
    links::group_dangling(rows)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM project", [], |r| r.get(0))?)
}

pub fn open(conn: &Connection) -> Result<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT key, status, coalesce(goal, '') FROM project \
         WHERE status NOT IN ('done', 'aborted') ORDER BY status, key",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(crate::error::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_key_strips_markers_and_slugs() {
        assert_eq!(normalize_key("chronos"), "chronos");
        assert_eq!(normalize_key(" Chronos "), "chronos");
        assert_eq!(normalize_key("prj:chronos"), "chronos");
        assert_eq!(
            normalize_key("prj-chronos"),
            "chronos",
            "the legacy spelling is accepted on input"
        );
        assert_eq!(normalize_key("prj sessions"), "prj-sessions");
        assert_eq!(normalize_key("  Deep Work  "), "deep-work");
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
    fn hydrate_parses_its_three_levels_and_refuses_junk() {
        assert_eq!(Hydrate::parse("").unwrap(), Hydrate::None);
        assert_eq!(Hydrate::parse(" none ").unwrap(), Hydrate::None);
        assert_eq!(Hydrate::parse("SNIPPET").unwrap(), Hydrate::Snippet);
        assert_eq!(Hydrate::parse("full").unwrap(), Hydrate::Full);
        let err = Hydrate::parse("lots").unwrap_err();
        assert!(format!("{err}").contains("unknown hydrate"), "{err}");
    }

    #[test]
    fn preload_codec_keeps_order_and_drops_noise() {
        let keys = vec![
            "boot-note".to_string(),
            "  Chronos Rules ".to_string(),
            "boot-note".to_string(),
            String::new(),
            "   ".to_string(),
        ];
        assert_eq!(encode_preload(&keys).unwrap(), "boot-note,chronos-rules");
        assert_eq!(encode_preload(&[]).unwrap(), "");
        assert!(
            encode_preload(&["prj:chronos".to_string()]).is_err(),
            "a project marker is not a memory key"
        );
        assert_eq!(decode_preload(""), Vec::<String>::new());
        assert_eq!(
            decode_preload("a,b"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn owner_codec_keeps_names_free_and_stores_a_canonical_set() {
        assert_eq!(
            encode_owners(&["Wile E. Coyote".to_string()]).unwrap(),
            "Wile E. Coyote"
        );
        assert_eq!(
            encode_owners(&["James".to_string(), "Cline".to_string()]).unwrap(),
            "Cline,James"
        );
        assert_eq!(
            encode_owners(&["James".to_string(), "james".to_string()]).unwrap(),
            "James",
            "one person, one entry"
        );
        assert_eq!(
            encode_owners(&["".to_string(), "  ".to_string()]).unwrap(),
            ""
        );
        assert_eq!(
            encode_owners(&[]).unwrap(),
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
    fn a_comma_in_an_owner_name_is_refused() {
        let err = encode_owners(&["Smith, John".to_string()]).unwrap_err();
        assert!(format!("{err}").contains("comma"), "{err}");
        assert_eq!(
            decode_owners("Smith,John"),
            vec!["Smith".to_string(), "John".to_string()],
            "the legacy stored form can only read back trimmed -- the fix is to never write it"
        );
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
