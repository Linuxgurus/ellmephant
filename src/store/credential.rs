use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub const BOOT_ENV: &str = "ELLMEPHANT_IDENTITIES";

const IMPORTED: &str = "identities_imported";

pub fn digest(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn fresh_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| Error::Invalid(format!("no randomness: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn pen_id(conn: &Connection, name: &str) -> Result<i64> {
    conn.query_row(
        "SELECT id FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

fn pen_named(conn: &Connection, name: &str) -> Result<(i64, String, Option<String>, Option<i64>)> {
    conn.query_row(
        "SELECT id, name, ceased_at, parent_id FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

fn refuse_root(stored: &str, parent: Option<i64>) -> Result<()> {
    if parent.is_none() {
        return Err(Error::Invalid(format!(
            "{stored} is a root pen and cannot hold a credential: a root's standing changes by \
             configuration, not by an API call"
        )));
    }
    Ok(())
}

pub fn issue(conn: &Connection, pen: &str, issued_by: Option<&str>) -> Result<String> {
    let (id, stored, ceased, parent) = pen_named(conn, pen)?;
    if ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{stored} has been retired: a pen that has ceased cannot hold a credential"
        )));
    }
    refuse_root(&stored, parent)?;
    let exists: Option<i64> = conn
        .query_row(
            "SELECT pen_id FROM pen_credential WHERE pen_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Err(Error::Invalid(format!(
            "{stored} already has a credential: `mint` issues it once, and `pen rotate` is the remedy"
        )));
    }
    let by = match issued_by {
        Some(name) => Some(pen_id(conn, name)?),
        None => None,
    };
    let secret = fresh_secret()?;
    conn.execute(
        "INSERT INTO pen_credential (pen_id, hash, issued_by) VALUES (?1, ?2, ?3)",
        params![id, digest(&secret), by],
    )?;
    Ok(secret)
}

pub fn rotate(conn: &Connection, caller: Option<&str>, target: &str) -> Result<String> {
    let (id, stored, ceased, parent) = pen_named(conn, target)?;
    if ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{stored} has been retired: a pen that has ceased cannot hold a credential"
        )));
    }
    refuse_root(&stored, parent)?;
    let by = match caller {
        Some(name) => {
            let caller_id = pen_id(conn, name)?;
            if caller_id != id && !crate::store::is_ancestor(conn, caller_id, id)? {
                return Err(Error::Invalid(format!(
                    "{name} may not rotate {stored}: a pen rotates itself or a descendant, never a peer and never an ancestor"
                )));
            }
            Some(caller_id)
        }
        None => None,
    };
    let secret = fresh_secret()?;
    conn.execute(
        "INSERT INTO pen_credential (pen_id, hash, issued_by) VALUES (?1, ?2, ?3)
         ON CONFLICT(pen_id) DO UPDATE SET
             hash       = excluded.hash,
             issued_by  = excluded.issued_by,
             issued_at  = datetime('now'),
             rotated_at = datetime('now')",
        params![id, digest(&secret), by],
    )?;
    Ok(secret)
}

pub fn verify(conn: &Connection, claimed: &str, proof: &str) -> Result<Option<String>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT p.name, c.hash
               FROM pen p
               JOIN pen_credential c ON c.pen_id = p.id
              WHERE lower(p.name) = lower(?1)
                AND p.ceased_at IS NULL",
            params![claimed],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((stored, hash)) = row else {
        return Ok(None);
    };
    Ok(same(digest(proof).as_bytes(), hash.as_bytes()).then_some(stored))
}

pub fn import_env(conn: &Connection) -> Result<usize> {
    let raw = match std::env::var(BOOT_ENV) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Ok(0),
    };
    let already: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k = ?1", params![IMPORTED], |r| {
            r.get(0)
        })
        .optional()?;
    if already.is_some() {
        return Ok(0);
    }

    let tx = conn.unchecked_transaction()?;
    let mut parsed = 0usize;
    let mut imported = 0usize;
    for pair in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((name, secret)) = pair.split_once(':') else {
            tracing::warn!(entry = %pair, "identities: expected `name:secret`, skipped");
            continue;
        };
        let (name, secret) = (name.trim(), secret.trim());
        if name.is_empty() || secret.is_empty() {
            tracing::warn!(entry = %pair, "identities: empty name or secret, skipped");
            continue;
        }
        parsed += 1;
        let row: Option<(Option<i64>, Option<String>)> = tx
            .query_row(
                "SELECT parent_id, ceased_at FROM pen WHERE lower(name) = lower(?1)",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let id: i64 = match row {
            Some((_, Some(_))) => {
                tracing::warn!(name = %name, "identities: pen is retired; credential skipped");
                continue;
            }
            Some((None, _)) => {
                tracing::warn!(
                    name = %name,
                    "identities: root pen cannot hold a credential; skipped"
                );
                continue;
            }
            Some((Some(_), None)) => tx.query_row(
                "SELECT id FROM pen WHERE lower(name) = lower(?1)",
                params![name],
                |r| r.get(0),
            )?,
            None => {
                let root: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM pen WHERE parent_id IS NULL ORDER BY id LIMIT 1",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                let Some(root) = root else {
                    tracing::warn!(name = %name, "identities: no root pen to attach to; skipped");
                    continue;
                };
                tx.execute(
                    "INSERT INTO pen (name, kind, parent_id) VALUES (?1, 'self', ?2)",
                    params![name, root],
                )?;
                tracing::warn!(
                    name = %name,
                    parent_id = root,
                    "identities: no pen row; attached under the root so it stays retirable"
                );
                tx.last_insert_rowid()
            }
        };
        imported += tx.execute(
            "INSERT OR IGNORE INTO pen_credential (pen_id, hash, issued_by) VALUES (?1, ?2, NULL)",
            params![id, digest(secret)],
        )?;
    }
    if parsed > 0 {
        tx.execute(
            "INSERT INTO meta (k, v) VALUES (?1, datetime('now'))
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![IMPORTED],
        )?;
    }
    tx.commit()?;
    if imported > 0 {
        tracing::info!(
            imported,
            env = BOOT_ENV,
            "identities: credentials imported; the table is authoritative from now on"
        );
    }
    Ok(imported)
}
