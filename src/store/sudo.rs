//! SUDO: authority borrowed, bounded, revocable (#109, round 6).
//!
//! The tree hands a pen its own subtree by right; anything wider must be BORROWED. `estate` is the
//! widest case and the reason the mechanism exists: it is a real pen with **no entity behind it** —
//! no identity entry, no credential, none coming — so it can never be a caller, and its universal
//! reach is only usable by someone borrowing it.
//!
//! Four checks, and each one exists because of a specific failure:
//!
//! - **a policy must exist** (`sudoers`), so borrowing is never implicit;
//! - **duration ≤ the policy's ceiling**, so a session cannot outlive the permission;
//! - **pass downward only** — a session may be handed to a pen's own descendant, never to a peer and
//!   never back up, or one hand could arm another hand's peer;
//! - **revocation cascades** — a session passed down dies with the session it came from, and killing
//!   the policy kills the sessions it authorised, or "revoked" is a word with a five-minute delay.
//!
//! ★ NO CACHE. Every check reads the table, because a cached session is a revocation that has not
//! happened yet. The cost is one indexed row read on a write that is already doing far more.
//!
//! ★ `purge` NEVER TRAVELS BY SUDO. Some acts are not delegable at all, and the one that destroys
//! history is the first of them; the rule lives at the call site that would otherwise use it.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};

/// A borrowing in progress: `weak` may act as `powerful` until `expires_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The `active_sudo` row.
    pub id: i64,
    /// The pen whose authority is borrowed.
    pub powerful: String,
    /// The pen doing the acting.
    pub weak: String,
    /// When it stops, as the store's own datetime text (`datetime('now')` format).
    pub expires_at: String,
    /// The session it was passed from, if it was.
    pub via: Option<i64>,
}

/// The largest duration any single session may be granted, in seconds (24 h).
///
/// Not a policy; a ceiling on policies. A `sudo` whose whole point is "cluster-wide hygiene once"
/// does not need a week, and a week-long session is a credential.
pub const MAX_DURATION_SECS: i64 = 86_400;

fn id_of(conn: &Connection, name: &str) -> Result<i64> {
    conn.query_row("SELECT id FROM pen WHERE name = ?1", params![name], |r| {
        r.get(0)
    })
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

/// Record that `weak` may borrow `powerful` for at most `max_duration_secs`. Provisioning, not a verb:
/// whoever can write this row can hand out authority, which is why `source` is required and stored.
pub fn grant(
    conn: &Connection,
    powerful: &str,
    weak: &str,
    max_duration_secs: i64,
    source: &str,
) -> Result<()> {
    if powerful == weak {
        return Err(Error::Invalid(format!(
            "{powerful} cannot borrow from itself: a pen's own authority is not borrowed, it is had"
        )));
    }
    if !(1..=MAX_DURATION_SECS).contains(&max_duration_secs) {
        return Err(Error::Invalid(format!(
            "a sudo policy must last between 1 and {MAX_DURATION_SECS} seconds (asked for {max_duration_secs})"
        )));
    }
    let (p, w) = (id_of(conn, powerful)?, id_of(conn, weak)?);
    conn.execute(
        "INSERT INTO sudoers (powerful, weak, max_duration_secs, source) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (powerful, weak) DO UPDATE SET max_duration_secs = ?3, source = ?4",
        params![p, w, max_duration_secs, source],
    )?;
    Ok(())
}

/// Remove a policy AND every session it authorised, in one transaction.
///
/// Both halves are one act: a policy removed while its sessions keep running is a revocation that
/// has not happened yet, and the sessions would be indistinguishable from permission.
pub fn revoke(conn: &Connection, powerful: &str, weak: &str) -> Result<usize> {
    let (p, w) = (id_of(conn, powerful)?, id_of(conn, weak)?);
    let tx = conn.unchecked_transaction()?;
    let killed = tx.execute(
        "DELETE FROM active_sudo WHERE powerful = ?1 AND weak = ?2",
        params![p, w],
    )?;
    tx.execute(
        "DELETE FROM sudoers WHERE powerful = ?1 AND weak = ?2",
        params![p, w],
    )?;
    tx.commit()?;
    Ok(killed)
}

/// Borrow `powerful`'s authority as `weak` for `secs`. A session, not a permission: it expires.
pub fn sudo(conn: &Connection, weak: &str, powerful: &str, secs: i64) -> Result<Session> {
    if weak == powerful {
        return Err(Error::Invalid(format!("{weak} cannot sudo to itself")));
    }
    let (p, w) = (id_of(conn, powerful)?, id_of(conn, weak)?);
    let ceiling: Option<i64> = conn
        .query_row(
            "SELECT max_duration_secs FROM sudoers WHERE powerful = ?1 AND weak = ?2",
            params![p, w],
            |r| r.get(0),
        )
        .optional()?;
    let Some(ceiling) = ceiling else {
        return Err(Error::Invalid(format!(
            "no sudo policy lets {weak} borrow {powerful}: authority is granted, never assumed"
        )));
    };
    if secs <= 0 || secs > ceiling {
        return Err(Error::Invalid(format!(
            "{weak} may borrow {powerful} for at most {ceiling}s (asked for {secs})"
        )));
    }
    conn.execute(
        "INSERT INTO active_sudo (powerful, weak, expires_at)
         VALUES (?1, ?2, datetime('now', ?3))",
        params![p, w, format!("+{secs} seconds")],
    )?;
    let id = conn.last_insert_rowid();
    let expires_at: String = conn.query_row(
        "SELECT expires_at FROM active_sudo WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    Ok(Session {
        id,
        powerful: powerful.to_string(),
        weak: weak.to_string(),
        expires_at,
        via: None,
    })
}

/// Hand a live session to a DESCENDANT of the pen holding it, for no longer than it has left.
pub fn pass(conn: &Connection, weak: &str, powerful: &str, to: &str, secs: i64) -> Result<Session> {
    let parent = live(conn, weak, powerful)?.ok_or_else(|| {
        Error::Invalid(format!(
            "{weak} holds no live {powerful} session to pass: a borrow cannot be lent by one who is not holding it"
        ))
    })?;
    let to_id = id_of(conn, to)?;
    let weak_id = id_of(conn, weak)?;
    if to_id == weak_id {
        return Err(Error::Invalid(
            "a session cannot be passed to the pen that already holds it".into(),
        ));
    }
    if !is_below(conn, weak_id, to_id)? {
        return Err(Error::Invalid(format!(
            "{to} is not a descendant of {weak}: a session may be passed DOWN, never sideways or up"
        )));
    }
    let remaining: i64 = conn.query_row(
        "SELECT CAST(max(0, julianday(?1) - julianday('now')) * 86400 AS INTEGER)",
        params![parent.expires_at],
        |r| r.get(0),
    )?;
    if secs <= 0 || secs > remaining {
        return Err(Error::Invalid(format!(
            "{to} may hold {powerful} for at most {remaining}s — a passed session cannot outlive the one it came from"
        )));
    }
    conn.execute(
        "INSERT INTO active_sudo (powerful, weak, expires_at, via)
         VALUES ((SELECT id FROM pen WHERE name = ?1), ?2, datetime('now', ?3), ?4)",
        params![powerful, to_id, format!("+{secs} seconds"), parent.id],
    )?;
    let id = conn.last_insert_rowid();
    let expires_at: String = conn.query_row(
        "SELECT expires_at FROM active_sudo WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )?;
    Ok(Session {
        id,
        powerful: powerful.to_string(),
        weak: to.to_string(),
        expires_at,
        via: Some(parent.id),
    })
}

/// Is `descendant` below `ancestor` in the pen tree? (A pen is not its own descendant here.)
fn is_below(conn: &Connection, ancestor: i64, descendant: i64) -> Result<bool> {
    let mut cur = descendant;
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

/// The live session, or `None`. Read from the table every time — never cached.
pub fn live(conn: &Connection, weak: &str, powerful: &str) -> Result<Option<Session>> {
    let Ok(w) = id_of(conn, weak) else {
        return Ok(None);
    };
    let Ok(p) = id_of(conn, powerful) else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT id, expires_at, via FROM active_sudo
              WHERE weak = ?1 AND powerful = ?2 AND expires_at > datetime('now')
              ORDER BY id DESC LIMIT 1",
            params![w, p],
            |r| {
                Ok(Session {
                    id: r.get(0)?,
                    powerful: powerful.to_string(),
                    weak: weak.to_string(),
                    expires_at: r.get(1)?,
                    via: r.get(2)?,
                })
            },
        )
        .optional()?)
}

/// End one session, and every session passed down from it. Returns how many died.
pub fn release(conn: &Connection, weak: &str, powerful: &str) -> Result<usize> {
    let w = id_of(conn, weak)?;
    let p = id_of(conn, powerful)?;
    let tx = conn.unchecked_transaction()?;
    let killed = tx.execute(
        "WITH RECURSIVE chain(id) AS (
             SELECT id FROM active_sudo WHERE weak = ?1 AND powerful = ?2
             UNION
             SELECT child.id FROM active_sudo child JOIN chain ON child.via = chain.id
         )
         DELETE FROM active_sudo WHERE id IN (SELECT id FROM chain)",
        params![w, p],
    )?;
    tx.commit()?;
    Ok(killed)
}

/// End every session a pen holds, whatever the authority, cascading down each chain.
pub fn release_all(conn: &Connection, weak: &str) -> Result<usize> {
    let w = id_of(conn, weak)?;
    let tx = conn.unchecked_transaction()?;
    let killed = tx.execute(
        "WITH RECURSIVE chain(id) AS (
             SELECT id FROM active_sudo WHERE weak = ?1
             UNION
             SELECT child.id FROM active_sudo child JOIN chain ON child.via = chain.id
         )
         DELETE FROM active_sudo WHERE id IN (SELECT id FROM chain)",
        params![w],
    )?;
    tx.commit()?;
    Ok(killed)
}

/// Everything `weak` currently holds, live only, newest first.
pub fn held(conn: &Connection, weak: &str) -> Result<Vec<Session>> {
    let Ok(w) = id_of(conn, weak) else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT s.id, p.name, s.expires_at, s.via
           FROM active_sudo s JOIN pen p ON p.id = s.powerful
          WHERE s.weak = ?1 AND s.expires_at > datetime('now')
          ORDER BY s.id DESC",
    )?;
    let rows = stmt.query_map(params![w], |r| {
        Ok(Session {
            id: r.get(0)?,
            powerful: r.get(1)?,
            weak: weak.to_string(),
            expires_at: r.get(2)?,
            via: r.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Load policy from a boot-time spec: `powerful>weak:seconds` entries, comma-separated.
///
/// ★ NEVER FATAL. A store that refuses to open because one policy line is malformed is a store that
/// turns a typo into an outage — and the recovery rule is that a hobbled Cline is better than no
/// Cline. Rejected entries are counted and named for `/health`; the rest are applied.
pub fn load(conn: &Connection, spec: &str, source: &str) -> (usize, Vec<String>) {
    let mut loaded = 0;
    let mut rejected = Vec::new();
    for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let Some((pair, secs)) = entry.rsplit_once(':') else {
            rejected.push(format!("{entry}: expected `powerful>weak:seconds`"));
            continue;
        };
        let Some((powerful, weak)) = pair.split_once('>') else {
            rejected.push(format!("{entry}: expected `powerful>weak:seconds`"));
            continue;
        };
        let (powerful, weak) = (powerful.trim(), weak.trim());
        let Ok(secs) = secs.trim().parse::<i64>() else {
            rejected.push(format!(
                "{entry}: `{}` is not a number of seconds",
                secs.trim()
            ));
            continue;
        };
        match grant(conn, powerful, weak, secs, source) {
            Ok(()) => loaded += 1,
            Err(e) => rejected.push(format!("{entry}: {e}")),
        }
    }
    (loaded, rejected)
}

/// The environment key boot-time policy is read from — the ConfigMap key in the estate, an env var
/// everywhere else. Named here so the two cannot drift.
pub const BOOT_ENV: &str = "ELLMEPHANT_SUDOERS";

/// Apply the boot spec, if any. ★ NEVER FATAL (#109): a store that will not open because one policy
/// line is malformed turns a typo into an outage, and "a hobbled Cline is better than no Cline" is
/// the rule for exactly this moment. Returns `(loaded, rejected)` for the log and for `/health`.
pub fn load_at_boot(conn: &Connection) -> (usize, usize) {
    let Ok(spec) = std::env::var(BOOT_ENV) else {
        return (0, 0);
    };
    let (loaded, rejected) = load(conn, &spec, "boot");
    for why in &rejected {
        tracing::warn!(spec = %why, "sudoers: entry rejected, the rest were applied");
    }
    tracing::info!(
        loaded,
        rejected = rejected.len(),
        "sudoers: {loaded} loaded, {} rejected",
        rejected.len()
    );
    (loaded, rejected.len())
}
