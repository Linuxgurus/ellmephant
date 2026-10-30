//! CONFIG: a stored value with a WRITTEN ACL (migration `0038`).
//!
//! A memory is a document a pen edits freely, and its `audience` protects READS only. Configuration
//! is the other shape — a small answer to "how should this run" — and it has TWO boundaries: who may
//! CHANGE the value, and who may SEE it. A memory row cannot express a write boundary at all.
//!
//! The shape is borrowed from `sudoers`: policy in columns, one row per item, and the reason recorded
//! on the row. The audience grammar is borrowed from [`super::audience`] rather than reinvented,
//! because a second grammar for a permission is a second place to get the boundary wrong.
//!
//! ★ TWO AUDIENCES, TWO QUESTIONS.
//! - `set_audience` — who may write `value`;
//! - `read_audience` — who may read the row.
//!
//! Both are audience values, so the SAME [`Reader`] answers both: `sees(set_audience)` is the write
//! gate and `sees(read_audience)` is the read gate. Nothing here re-implements ancestry or borrowing.
//!
//! ★ NO AMPLIFICATION. `define` may only name addresses the caller can reach (itself or a pen below
//! it, or anything when it holds the root). A sibling is not a share, and a boundary a caller can
//! name is a boundary an injection picks.
//!
//! ★ ABSENCE, NOT REFUSAL. A caller who may not read an item gets [`None`] from [`get`] and no row
//! from [`list`] — the same rule as a private memory. A refusal would confirm the item exists, which
//! is the disclosure. A write refusal *is* reported: the caller asked to change something, and
//! silence there would look like success.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::store::audience::{self, Reader};

/// Longest key we accept. A config name is an address, not a document.
pub const MAX_KEY_LEN: usize = 128;

/// Largest value we accept (64 KiB). Configuration is small; a large value belongs in a memory or a
/// project, where the store's search and versioning are built for it.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;

/// One config item, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// The item's name, lowercased.
    pub key: String,
    /// The stored value. Text on purpose: the consumer parses its own units (`2h`, `true`, `ste`).
    pub value: String,
    /// Who may write [`Item::value`].
    pub set_audience: String,
    /// Who may read this row.
    pub read_audience: String,
    /// The pen that set the CURRENT value, when one is recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// When the value last changed.
    pub updated_at: String,
    /// Why this item exists / where the value came from.
    pub source: String,
}

/// The chosen columns, with `owner` resolved to a pen NAME (a retired pen still has a name).
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

/// Normalise a key: trimmed, lowercased, non-empty, no whitespace, and short.
///
/// Not slugified: `recess.daily_time` and `recess_daily_time` are both legitimate names an operator
/// may choose. Case folds because two spellings of one setting are one setting.
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

/// A value must be present. Empty is not a default — a caller that wants "unset" should delete the
/// item, and an empty string would silently be one more value to parse.
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

/// Read one item. `None` means "no such item OR not addressed to you" — the same absence a private
/// memory returns. A caller that needs to distinguish them is asking to probe the ACL, and it may not.
pub fn get(conn: &Connection, reader: &Reader, key: &str) -> Result<Option<Item>> {
    let key = normalize_key(key)?;
    Ok(load(conn, &key)?.filter(|item| reader.sees(&item.read_audience)))
}

/// Every item this reader may see, key order. An optional `prefix` filters by name, which is what
/// makes a namespace (`recess.` / `recess_`) usable without a second finder.
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

/// The pen row for a name, when it exists. Used only to stamp `owner`.
fn pen_id(conn: &Connection, name: &str) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| r.get(0),
    )
    .optional()
    .map_err(Error::from)
}

/// Change an EXISTING item's value. The ACL is untouched: changing who may set an item is
/// [`define`], a provisioning act, not a value write.
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

/// Create an item, or change its ACL and value. This is the PROVISIONING act: it may hand out
/// authority, so it is gated harder than [`set`].
///
/// Two rules, both there to stop a pen manufacturing reach:
/// 1. every address named in either audience must be reachable by the caller (itself, a descendant,
///    or anything under the root's break-glass);
/// 2. an EXISTING item may only be redefined by a caller who could already write it.
///
/// `set_audience` may not be `public`: a value anyone can write is not configuration.
#[allow(clippy::too_many_arguments)] // one row, one act: splitting it would only move the fields
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

    // Rule 1 — you may only grant what you can reach.
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

    // Rule 2 — an existing item is policy; only a caller who could write it may rewrite the policy.
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

/// May this reader address `name` — name itself, a pen below it, or anything under the root?
///
/// Deliberately NOT [`Reader::sees`]: that asks which addresses a reader HOLDS (its ancestors and
/// borrowed authority), which is the opposite direction. Granting downward is a question about the
/// reader's subtree, so it is asked of the pen table directly.
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
    // Walk UP from `name`; if `me` appears, `me` is above it (the parentage grant).
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
