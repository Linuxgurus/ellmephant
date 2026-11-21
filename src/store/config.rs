use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::audience::{self, Reader};

pub const MAX_KEY_LEN: usize = 128;

pub const MAX_VALUE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub key: String,
    pub value: String,
    pub set_audience: String,
    pub read_audience: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub updated_at: String,
    pub source: String,
}

const COLUMNS: &str =
    "c.key, c.value, c.set_audience, c.read_audience, p.name, c.updated_at, c.source";

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        key: r.get(0)?,
        value: r.get(1)?,
        set_audience: r.get(2)?,
        read_audience: r.get(3)?,
        owner: r.get(4)?,
        updated_at: r.get(5)?,
        source: r.get(6)?,
    })
}

pub fn normalize_key(raw: &str) -> Result<String> {
    let key = raw.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Err(Error::Invalid(
            "a config item needs a non-empty `key`".into(),
        ));
    }
    if key.len() > MAX_KEY_LEN {
        return Err(Error::Invalid(format!(
            "config key is {} bytes; the limit is {MAX_KEY_LEN}",
            key.len()
        )));
    }
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::Invalid(format!(
            "`{raw}` is not a config key: no whitespace or control characters"
        )));
    }
    Ok(key)
}

pub fn normalize_value(raw: &str) -> Result<String> {
    if raw.trim().is_empty() {
        return Err(Error::Invalid(
            "a config value may not be empty; omit the item instead of storing a blank".into(),
        ));
    }
    if raw.len() > MAX_VALUE_BYTES {
        return Err(Error::TooLarge(format!(
            "config value is {} bytes; the limit is {MAX_VALUE_BYTES}",
            raw.len()
        )));
    }
    Ok(raw.to_string())
}

fn load(conn: &Connection, key: &str) -> Result<Option<Item>> {
    conn.query_row(
        &format!(
            "SELECT {COLUMNS} FROM config c LEFT JOIN pen p ON p.id = c.owner WHERE c.key = ?1"
        ),
        params![key],
        row_to_item,
    )
    .optional()
    .map_err(Error::from)
}

pub fn get(conn: &Connection, reader: &Reader, key: &str) -> Result<Option<Item>> {
    let key = normalize_key(key)?;
    Ok(load(conn, &key)?.filter(|item| reader.sees(&item.read_audience)))
}

pub fn list(conn: &Connection, reader: &Reader, prefix: Option<&str>) -> Result<Vec<Item>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM config c LEFT JOIN pen p ON p.id = c.owner ORDER BY c.key"
    ))?;
    let rows = stmt.query_map([], row_to_item)?;
    let prefix = prefix.map(|p| p.trim().to_ascii_lowercase());
    let mut out = Vec::new();
    for row in rows {
        let item = row?;
        if prefix
            .as_deref()
            .is_some_and(|p| !p.is_empty() && !item.key.starts_with(p))
        {
            continue;
        }
        if reader.sees(&item.read_audience) {
            out.push(item);
        }
    }
    Ok(out)
}

fn pen_id(conn: &Connection, name: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| r.get(0),
    )
    .optional()
    .map_err(Error::from)
}

pub fn set(
    conn: &Connection,
    reader: &Reader,
    who: &str,
    key: &str,
    value: &str,
    source: Option<&str>,
) -> Result<Item> {
    let key = normalize_key(key)?;
    let value = normalize_value(value)?;
    let Some(old) = load(conn, &key)? else {
        return Err(Error::NotFound(format!(
            "no config `{key}` — define it first (`config_define`), because a value with no written ACL \
             would be whoever happens to write it first"
        )));
    };
    if !reader.sees(&old.set_audience) {
        return Err(Error::Forbidden(format!(
            "{who} may not set `{key}`: its `set_audience` is `{}`",
            old.set_audience
        )));
    }
    let owner = pen_id(conn, who)?;
    conn.execute(
        "UPDATE config
            SET value = ?2,
                owner = ?3,
                updated_at = datetime('now'),
                source = CASE WHEN ?4 IS NULL OR trim(?4) = '' THEN source ELSE ?4 END
          WHERE key = ?1",
        params![key, value, owner, source],
    )?;
    load(conn, &key)?
        .ok_or_else(|| Error::NotFound(format!("config `{key}` vanished while writing it")))
}

/// Remove an item, gated by its current `set_audience` — the same boundary `set` uses.
///
/// Returns the item as it stood, or `None` when there was no such item (an unset of something
/// absent is already the requested state, not an error). The row's value and `source` are gone
/// afterwards: `config` keeps no versions, which is why this is a separate, deliberate act.
pub fn unset(conn: &Connection, reader: &Reader, who: &str, key: &str) -> Result<Option<Item>> {
    let key = normalize_key(key)?;
    let Some(old) = load(conn, &key)? else {
        return Ok(None);
    };
    if !reader.sees(&old.set_audience) {
        return Err(Error::Forbidden(format!(
            "{who} may not unset `{key}`: its `set_audience` is `{}`",
            old.set_audience
        )));
    }
    let removed = conn.execute("DELETE FROM config WHERE key = ?1", params![key])?;
    if removed == 0 {
        return Ok(None);
    }
    Ok(Some(old))
}

#[allow(clippy::too_many_arguments)]
pub fn define(
    conn: &Connection,
    reader: &Reader,
    who: Option<&str>,
    key: &str,
    value: &str,
    set_audience: &str,
    read_audience: &str,
    source: &str,
) -> Result<Item> {
    let key = normalize_key(key)?;
    let value = normalize_value(value)?;
    let set_aud = audience::validated(conn, set_audience)?;
    let read_aud = audience::validated(conn, read_audience)?;
    if set_aud == audience::PUBLIC {
        return Err(Error::Invalid(
            "`set_audience` may not be `public`: a value anyone can write is not configuration"
                .into(),
        ));
    }
    let source = source.trim();
    if source.is_empty() {
        return Err(Error::Invalid(
            "`config_define` needs a `source`: configuration is the trust root, and the reason for a \
             grant belongs on the row"
                .into(),
        ));
    }

    let named = audience::parse(&set_aud)
        .unwrap_or_default()
        .into_iter()
        .chain(audience::parse(&read_aud).unwrap_or_default());
    for target in named {
        if !may_address(conn, reader, &target.name)? {
            return Err(Error::Forbidden(format!(
                "may not name `{}` in a config audience: a pen can only grant what it can reach \
                 (itself, its subtree, or under the root's break-glass)",
                target.name
            )));
        }
    }

    if let Some(old) = load(conn, &key)?
        && !reader.sees(&old.set_audience)
    {
        return Err(Error::Forbidden(format!(
            "may not redefine `{key}`: its current `set_audience` is `{}`",
            old.set_audience
        )));
    }

    let owner = who.and_then(|w| pen_id(conn, w).ok().flatten());
    conn.execute(
        "INSERT INTO config (key, value, set_audience, read_audience, owner, updated_at, source)
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), ?6)
         ON CONFLICT (key) DO UPDATE
            SET value = ?2,
                set_audience = ?3,
                read_audience = ?4,
                owner = ?5,
                updated_at = datetime('now'),
                source = ?6",
        params![key, value, set_aud, read_aud, owner, source],
    )?;
    load(conn, &key)?
        .ok_or_else(|| Error::NotFound(format!("config `{key}` vanished while writing it")))
}

fn may_address(conn: &Connection, reader: &Reader, name: &str) -> Result<bool> {
    if reader.is_root() {
        return Ok(true);
    }
    let Some(me) = reader.pen_name() else {
        return Ok(false);
    };
    if me.eq_ignore_ascii_case(name) {
        return Ok(true);
    }
    let above: bool = conn.query_row(
        "WITH RECURSIVE up(id, parent_id) AS (
             SELECT id, parent_id FROM pen WHERE lower(name) = lower(?1)
             UNION
             SELECT p.id, p.parent_id FROM pen p JOIN up u ON p.id = u.parent_id
         )
         SELECT EXISTS(
             SELECT 1 FROM pen p JOIN up u ON p.id = u.id WHERE lower(p.name) = lower(?2)
         )",
        params![name, me],
        |r| r.get(0),
    )?;
    Ok(above)
}
