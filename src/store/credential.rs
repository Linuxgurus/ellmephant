//! Credentials as rows: a pen's secret, hashed, plus the one-shot import that retires
//! `ELLMEPHANT_IDENTITIES` (2026-10-30).
//!
//! A name used to be proved by a process variable, re-read every call. The store could not revoke
//! it, could not rotate it without a deploy, and — the sharp edge — could not stop a RETIRED pen
//! from authenticating, because the authentication path never looked at the `pen` row. Credentials
//! are now the second registry folded into the first: a row belongs to a pen, dies with its
//! retirement, and is replaced in place.
//!
//! ★ THE PLAINTEXT EXISTS ONCE. `issue` and `rotate` return it; the table holds only the lowercase
//! hex of SHA-256. A getter would make the store a password file; a rotation is the way to recover
//! a lost secret.
//!
//! ★ NO CACHE, LIKE SUDO. Every authentication reads the row, so a rotation or a retirement takes
//! effect on the very next call.

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// The environment key that once held `name:secret` pairs. It is now read ONCE, by [`import_env`],
/// so an existing store keeps working across the upgrade; after that the table is authoritative.
pub const BOOT_ENV: &str = "ELLMEPHANT_IDENTITIES";

/// The `meta` marker that makes the import one-shot. Without it, every boot would re-create a
/// credential an ancestor deliberately deleted — a backdoor with a friendly name.
const IMPORTED: &str = "identities_imported";

/// Lowercase hex SHA-256. Public so a provisioning tool can seed a credential it already holds.
pub fn digest(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 32 random bytes as 64 hex characters. Entropy is the defence: no password KDF is needed, and one
/// on this path would make every authenticated call expensive.
pub fn fresh_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| Error::Invalid(format!("no randomness: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Length-aware, branch-free comparison — no early exit for a wrong byte.
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A pen's id by name, folding case — the same rule every other layer uses (migration 0027).
fn pen_id(conn: &Connection, name: &str) -> Result<i64> {
    conn.query_row(
        "SELECT id FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

/// The pen's stored spelling and whether it has ceased — so a refusal names the row, not the
/// caller's typing.
fn pen_named(conn: &Connection, name: &str) -> Result<(i64, String, Option<String>)> {
    conn.query_row(
        "SELECT id, name, ceased_at FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

/// Issue a credential for a pen that has none. Returns the plaintext ONCE.
///
/// Refuses a pen that already has one: `mint` has just created the row, so a collision means a bug
/// or a replayed call, and silently replacing a live secret is exactly the kind of "helpful" act
/// that locks somebody out.
pub fn issue(conn: &Connection, pen: &str, issued_by: Option<&str>) -> Result<String> {
    let (id, stored, ceased) = pen_named(conn, pen)?;
    if ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{stored} has been retired: a pen that has ceased cannot hold a credential"
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

/// Replace a pen's secret, returning the new plaintext ONCE. The old one stops working on the next
/// call because authentication reads the row.
///
/// `caller = None` is the PROVISIONING door (CLI / kubectl exec): whoever can open the database can
/// already read every hash. `Some(caller)` is the runtime door: a pen may rotate itself, and an
/// ancestor may rotate a descendant — never a peer and never an ancestor.
pub fn rotate(conn: &Connection, caller: Option<&str>, target: &str) -> Result<String> {
    let (id, stored, ceased) = pen_named(conn, target)?;
    if ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{stored} has been retired: a pen that has ceased cannot hold a credential"
        )));
    }
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

/// Verify a claimed name and proof. Returns the pen's STORED spelling on success.
///
/// A retired pen answers `None`: the secret may still be in a config somewhere, and it must stop
/// working the moment `ceased_at` is set. A pen with no credential row also answers `None` — that is
/// the honest state of `estate`.
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

/// Read `ELLMEPHANT_IDENTITIES` ONCE and hash it into rows, then mark the import done.
///
/// Idempotent in the way that matters: a deleted credential is never resurrected by a later boot.
/// Returns how many credentials were created. A malformed pair is skipped with a warning and named;
/// one bad line never costs the rest, and the store never refuses to open over a config typo.
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
        // A name in the old table may have no row yet (a self that has never written). The pen
        // registry is the authority for existence, so register it exactly as `ensure_pen` would.
        tx.execute(
            "INSERT OR IGNORE INTO pen (name, kind) VALUES (?1, 'self')",
            params![name],
        )?;
        let id: i64 = tx.query_row(
            "SELECT id FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| r.get(0),
        )?;
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
