//! Claims: an atomic **lease over a NAME** (migration `0012`).
//!
//! Two hands sharing one store must not walk over each other around a read-modify-write. The bank's
//! `debit` is the case that bites — two hands both read `$0.95` and both spend it — but the pattern is
//! general: read a value, decide, write it back, and two writers can interleave. A claim is a named
//! lease: one owner at a time, time-bounded, enforced by the store itself.
//!
//! WHY THE STORE, AND NOT A CLIENT. The store's tool layer *versions* a row — it keeps the previous
//! state, last-write-wins — it does not *transact*. A lease built from `memory_get` + `memory_upsert`
//! is a read-modify-write, so two clients can both read "free" and both write "mine"; acquiring is
//! itself the read-modify-write, which is exactly why a client-side lease is racy by construction.
//! [`acquire`] is therefore ONE SQL statement — `INSERT … ON CONFLICT … DO UPDATE … WHERE` — whose
//! decision and write the database makes atomically. No clock orders the tie-break: the design note
//! `finger-claim-protocol` records a ~7h skew between two hands, so the ordering is IDENTITY (who the
//! row says holds it) plus EXPIRY (a deadline that frees it with no help).
//!
//! WHAT A CLAIM IS NOT: **security**. Both hands are the same self — no adversary, no token, no crypto;
//! this is coordination only, two writers kept apart. It is also **not a lock that must be released**:
//! [`release`] is the tidy path for a holder that finished, but a lease that lapses is harmless, and an
//! expired row is left in place on purpose so [`list`] can SHOW an unexplained stale claim rather than
//! hide it — an unreadable bug report is the failure mode this exists to prevent.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Lease length when a caller does not ask for one, in seconds — 15 minutes: long enough for a hand to
/// launch a finger and debit the bank inside the critical section, short enough that a crash frees the
/// name soon.
pub const DEFAULT_TTL_SECONDS: i64 = 900;

/// Longest lease a caller may take, in seconds (24h). A lease that outlives the buyer is a leak.
pub const MAX_TTL_SECONDS: i64 = 86_400;

/// One claim row as stored, with the derived `live` flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    /// The name the lease is over — free text (a bank key, a handoff topic, anything two writers share).
    pub name: String,
    /// Who holds it — lowercased, so one identity cannot wear two spellings.
    pub owner: String,
    /// When this holder FIRST took it — renewals do not reset it, so "how long held" stays answerable.
    pub acquired_at: String,
    /// When it lapses on its own.
    pub expires_at: String,
    /// True while the lease has not expired.
    pub live: bool,
}

/// The result of an [`acquire`]: whether you won, and who holds the name NOW (you, if you won).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Acquire {
    /// True when this call took or renewed the lease.
    pub won: bool,
    /// The holder after the attempt — always present: the win, or the one that beat you.
    pub claim: Claim,
}

/// The result of a [`release`]: whether it dropped, and what is left (the next holder, or nothing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    /// True when the lease was dropped.
    pub released: bool,
    /// The lease still standing, when one is (`None` after a clean release).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
    /// ★ WHO DROPPED IT, when it was not the holder. An ancestor may end a descendant's lease with a
    /// stated reason (soma/ellmephant, soma/spine#43); a release that says who did it is an act, one
    /// that does not is an accident.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_by: Option<String>,
    /// The reason the ancestor gave. Present only with `released_by`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The chosen columns, with `live` derived in SQL so a reader never compares timestamps itself.
const COLUMNS: &str = "name, owner, acquired_at, expires_at, (expires_at > datetime('now'))";

/// Map one `claim` row (the [`COLUMNS`] list) to a [`Claim`].
fn row_to_claim(r: &rusqlite::Row<'_>) -> rusqlite::Result<Claim> {
    Ok(Claim {
        name: r.get(0)?,
        owner: r.get(1)?,
        acquired_at: r.get(2)?,
        expires_at: r.get(3)?,
        live: r.get(4)?,
    })
}

/// Normalize a lease name: trimmed, lowercased, non-empty. Free text (NOT slugified) — a name is a
/// lookup handle, not a document key, so punctuation is allowed. Case is folded because `Bank` and
/// `bank` must be one lease: identity is the point of the row, and a spelling is not a holder.
fn normalize_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(Error::Claim("a claim needs a non-empty `name`".to_string()));
    }
    Ok(name.to_lowercase())
}

/// Normalize an owner: trimmed, lowercased, non-empty. Folding case matters more here than for the
/// name: `Hand-A` renewing a lease it took as `hand-a` would otherwise be refused — deadlocking a hand
/// on its own lease.
fn normalize_owner(raw: &str) -> Result<String> {
    let owner = raw.trim();
    if owner.is_empty() {
        return Err(Error::Claim(
            "a claim needs a non-empty `owner`".to_string(),
        ));
    }
    Ok(owner.to_lowercase())
}

/// Normalize a TTL: the default when unstated, and bounded so a lease cannot be forever.
fn normalize_ttl(ttl_seconds: Option<i64>) -> Result<i64> {
    let ttl = ttl_seconds.unwrap_or(DEFAULT_TTL_SECONDS);
    if ttl <= 0 {
        return Err(Error::Claim(format!(
            "ttl_seconds must be positive (got {ttl})"
        )));
    }
    if ttl > MAX_TTL_SECONDS {
        return Err(Error::Claim(format!(
            "ttl_seconds may not exceed {MAX_TTL_SECONDS} (got {ttl}) — a lease that outlives its buyer is a leak"
        )));
    }
    Ok(ttl)
}

/// Acquire (or renew) a lease on `name` for `owner`.
///
/// **Atomic by construction:** one statement decides and writes. The `WHERE` on the upsert lets the
/// write through only when the held lease has EXPIRED or `owner` already holds it — so a live lease
/// held by someone else is refused, and the same owner re-taking it is a RENEWAL (idempotent), not a
/// conflict. A renewal moves the deadline but keeps the FIRST `acquired_at`, so the age of the lease
/// stays readable. `RETURNING` hands back the row this statement wrote, so a win is reported from the
/// write itself and never from a second read another writer could have moved.
///
/// The caller learns *whether* it won and *who holds the name now*: a `won: false` reply carries the
/// holder, because "you lost" without "to whom, until when" is not actionable. A loss is resolved by
/// reading the row — and when that row has VANISHED (the holder released between our failed write and
/// our read), the acquire is retried ONCE: the name just became free, so the honest answer is a win,
/// not an error. The error survives only as the last resort, when the retry loses and the row vanishes
/// again.
pub fn acquire(
    conn: &Connection,
    name: &str,
    owner: &str,
    ttl_seconds: Option<i64>,
) -> Result<Acquire> {
    let name = normalize_name(name)?;
    let owner = normalize_owner(owner)?;
    let ttl = normalize_ttl(ttl_seconds)?;
    let offset = format!("+{ttl} seconds");

    if let Some(claim) = try_acquire(conn, &name, &owner, &offset)? {
        return Ok(Acquire { won: true, claim });
    }
    // The only way to get here: a live lease held by a DIFFERENT owner. Report it — unless it is gone.
    match get(conn, &name)? {
        Some(claim) => Ok(Acquire { won: false, claim }),
        // Vanished between write and read: the name is free NOW, so try the write again once.
        None => match try_acquire(conn, &name, &owner, &offset)? {
            Some(claim) => Ok(Acquire { won: true, claim }),
            None => {
                let claim = get(conn, &name)?.ok_or_else(|| {
                    Error::Claim(format!("claim `{name}` vanished between write and read"))
                })?;
                Ok(Acquire { won: false, claim })
            }
        },
    }
}

/// The one-statement attempt: `Some` when the write won, `None` when a live lease by another refused
/// it. Never a second read — the win comes from `RETURNING`, i.e. from the write.
fn try_acquire(conn: &Connection, name: &str, owner: &str, offset: &str) -> Result<Option<Claim>> {
    Ok(conn
        .query_row(
            &format!(
                "INSERT INTO claim (name, owner, acquired_at, expires_at)
                 VALUES (?1, ?2, datetime('now'), datetime('now', ?3))
                 ON CONFLICT(name) DO UPDATE SET
                     owner = excluded.owner,
                     -- ★ A RENEWAL MAY EXTEND, NEVER SHORTEN. A live lease keeps the later deadline,
                     -- so a short probe cannot release a long lock early (measured 2026-10-17: a 60s
                     -- renewal truncated a 64-minute `finger` lock). A LAPSED lease is a different
                     -- case — it must take the new window, or it would come back already expired.
                     expires_at = CASE
                         WHEN claim.expires_at <= datetime('now') THEN excluded.expires_at
                         WHEN excluded.expires_at > claim.expires_at THEN excluded.expires_at
                         ELSE claim.expires_at
                     END
                 WHERE claim.expires_at <= datetime('now') OR claim.owner = excluded.owner
                 RETURNING {COLUMNS}"
            ),
            params![name, owner, offset],
            row_to_claim,
        )
        .optional()?)
}

/// Release a lease — only your own. A release by a non-owner is refused, and the reply (when one
/// remains) names the holder, so the caller learns who it is, not just that it failed.
pub fn release(conn: &Connection, name: &str, owner: &str) -> Result<Release> {
    release_by(conn, name, owner, None)
}

/// ★ RELEASE AS A PEN, NOT AS A STRING (soma/spine#43 fix 4). The holder may always release its own
/// lease. An ANCESTOR of the holder may also release it — but only with a stated reason, because a
/// guard that can only be cleared by the process it guards is a room that locks from the inside, and
/// a release nobody signed is indistinguishable from a leak. Authority is checked against the ROW's
/// owner, never against the `owner` a caller asserts.
pub fn release_by(
    conn: &Connection,
    name: &str,
    actor: &str,
    reason: Option<&str>,
) -> Result<Release> {
    let name = normalize_name(name)?;
    let actor = normalize_owner(actor)?;
    let Some(claim) = get(conn, &name)? else {
        return Ok(Release {
            released: false,
            claim: None,
            released_by: None,
            reason: None,
        });
    };
    let mine = claim.owner == actor;
    let ancestor = if mine {
        false
    } else {
        match (pen_id(conn, &actor)?, pen_id(conn, &claim.owner)?) {
            (Some(a), Some(o)) => crate::store::is_ancestor(conn, a, o)?,
            _ => false,
        }
    };
    if !mine && !ancestor {
        return Ok(Release {
            released: false,
            claim: Some(claim),
            released_by: None,
            reason: None,
        });
    }
    let stated = reason.map(str::trim).filter(|r| !r.is_empty());
    if ancestor && stated.is_none() {
        return Err(Error::Forbidden(
            "an ancestor may end a descendant's lease only with a `reason` (a quiet release is \
             indistinguishable from a leak)"
                .to_string(),
        ));
    }
    let dropped = conn.execute(
        "DELETE FROM claim WHERE name = ?1 AND owner = ?2",
        params![name, claim.owner],
    )?;
    Ok(Release {
        released: dropped > 0,
        claim: None,
        released_by: if ancestor { Some(actor.clone()) } else { None },
        reason: if ancestor {
            stated.map(str::to_string)
        } else {
            None
        },
    })
}

/// The pen id for a name, case-folded like every other identity lookup.
fn pen_id(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| r.get(0),
        )
        .optional()?)
}

/// One claim by name, or `None` when no row exists (a never-claimed name). The lookup folds case the
/// same way [`acquire`] does, so `claims(Some("Bank"))` finds the lease stored as `bank`.
pub fn get(conn: &Connection, name: &str) -> Result<Option<Claim>> {
    let name = name.trim().to_lowercase();
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM claim WHERE name = ?1"),
            params![name],
            row_to_claim,
        )
        .optional()?)
}

/// Every claim, name order — the audit view. Expired rows stay on purpose: an unexplained stale claim
/// is a bug report a reader can SEE, and hiding it would defeat the one thing this tool is for.
pub fn list(conn: &Connection) -> Result<Vec<Claim>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM claim ORDER BY name"))?;
    Ok(stmt
        .query_map([], row_to_claim)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    fn store() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_root(dir.path());
        let conn = crate::db::open(&paths).expect("open");
        (dir, conn)
    }

    #[test]
    fn a_free_name_is_won() {
        let (_dir, conn) = store();
        let out = acquire(&conn, "bank", "hand-a", None).expect("acquire");
        assert!(out.won);
        assert_eq!(out.claim.owner, "hand-a");
        assert!(out.claim.live);
    }

    #[test]
    fn a_live_lease_held_by_another_is_refused_and_named() {
        let (_dir, conn) = store();
        acquire(&conn, "bank", "hand-a", Some(300)).expect("first");
        let out = acquire(&conn, "bank", "hand-b", Some(300)).expect("second");
        assert!(!out.won, "a live lease must refuse the second writer");
        assert_eq!(out.claim.owner, "hand-a", "and name the holder");
    }

    #[test]
    fn the_same_owner_renewing_is_a_win() {
        let (_dir, conn) = store();
        acquire(&conn, "bank", "hand-a", Some(300)).expect("first");
        let out = acquire(&conn, "bank", "hand-a", Some(600)).expect("renew");
        assert!(out.won, "the holder may renew its own lease");
    }

    #[test]
    fn a_shorter_renewal_cannot_shorten_a_live_lease() {
        let (_dir, conn) = store();
        let long = acquire(&conn, "finger", "hands", Some(3840)).expect("long lease");
        // The bug this pins: a 60s probe used to truncate a 64-minute lock, releasing it early and
        // letting a second finger into the same work.
        let probe = acquire(&conn, "finger", "hands", Some(60)).expect("probe");
        assert!(probe.won, "the holder may renew");
        assert_eq!(
            probe.claim.expires_at, long.claim.expires_at,
            "a shorter TTL must not move the deadline backwards"
        );
        // ...and a longer one still extends it.
        let longer = acquire(&conn, "finger", "hands", Some(7200)).expect("extend");
        assert!(
            longer.claim.expires_at > long.claim.expires_at,
            "a longer TTL must extend the lease"
        );
    }

    #[test]
    fn an_expired_lease_is_taken_over() {
        let (_dir, conn) = store();
        acquire(&conn, "bank", "hand-a", Some(300)).expect("first");
        // A crash left the row; the deadline passes. Rewind it rather than sleep.
        conn.execute(
            "UPDATE claim SET expires_at = datetime('now', '-1 second') WHERE name = 'bank'",
            [],
        )
        .expect("expire");
        let out = acquire(&conn, "bank", "hand-b", Some(300)).expect("second");
        assert!(out.won, "an expired lease must not block");
        assert_eq!(out.claim.owner, "hand-b");
    }

    /// One pen under another — the shape `stet`/`hands` has. `estate` is seeded by migration 0021.
    fn pen(conn: &Connection, name: &str, parent: &str) {
        conn.execute(
            "INSERT INTO pen (name, kind, parent_id)
             VALUES (?1, 'self', (SELECT id FROM pen WHERE lower(name) = lower(?2)))",
            params![name, parent],
        )
        .expect("pen");
    }

    fn pens(conn: &Connection) {
        pen(conn, "stet", "estate");
        pen(conn, "hands", "stet");
        pen(conn, "twine", "estate");
    }

    #[test]
    fn the_holder_still_releases_its_own_lease() {
        let (_dir, conn) = store();
        pens(&conn);
        acquire(&conn, "finger", "hands", Some(300)).expect("acquire");
        let out = release_by(&conn, "finger", "hands", None).expect("own release");
        assert!(out.released);
        assert!(
            out.released_by.is_none(),
            "no attribution: the holder did it"
        );
    }

    #[test]
    // WITH a reason — the emphasis the old name carried in capitals.
    fn an_ancestor_may_end_a_descendants_lease_with_a_reason() {
        let (_dir, conn) = store();
        pens(&conn);
        acquire(&conn, "finger", "hands", Some(300)).expect("acquire");
        let out = release_by(&conn, "finger", "stet", Some("  the launcher was killed  "))
            .expect("ancestor release");
        assert!(
            out.released,
            "an ancestor may clear a dead descendant's lease"
        );
        assert_eq!(out.released_by.as_deref(), Some("stet"));
        assert_eq!(
            out.reason.as_deref(),
            Some("the launcher was killed"),
            "trimmed, and on the record"
        );
        assert!(get(&conn, "finger").expect("get").is_none(), "gone");
    }

    #[test]
    fn an_ancestor_without_a_reason_is_refused() {
        let (_dir, conn) = store();
        pens(&conn);
        acquire(&conn, "finger", "hands", Some(300)).expect("acquire");
        let refused = release_by(&conn, "finger", "stet", Some("   "));
        assert!(
            refused.is_err(),
            "a quiet ancestor release is indistinguishable from a leak"
        );
        assert!(
            get(&conn, "finger").expect("get").is_some(),
            "and nothing was dropped"
        );
    }

    #[test]
    fn a_pen_that_is_not_an_ancestor_is_refused_and_told_who_holds_it() {
        let (_dir, conn) = store();
        pens(&conn);
        acquire(&conn, "finger", "hands", Some(300)).expect("acquire");
        let out =
            release_by(&conn, "finger", "twine", Some("cousin's privilege")).expect("refused");
        assert!(!out.released, "a cousin is not an ancestor");
        assert_eq!(out.claim.expect("still held").owner, "hands");
    }

    #[test]
    fn the_owner_argument_cannot_grant_reach() {
        let (_dir, conn) = store();
        pens(&conn);
        acquire(&conn, "finger", "hands", Some(300)).expect("acquire");
        // Twine names the holder — the old shape of the hole. The row's owner decides, not the claim.
        let out = release_by(&conn, "finger", "twine", Some("I say I am hands")).expect("refused");
        assert!(
            !out.released,
            "authority is the proven pen, never the asserted owner"
        );
    }

    #[test]
    fn release_only_drops_your_own() {
        let (_dir, conn) = store();
        acquire(&conn, "bank", "hand-a", Some(300)).expect("first");
        let refused = release(&conn, "bank", "hand-b").expect("wrong owner");
        assert!(!refused.released, "a non-owner cannot release");
        assert_eq!(
            refused.claim.expect("still held").owner,
            "hand-a",
            "and the reply names the holder"
        );
        let dropped = release(&conn, "bank", "hand-a").expect("owner");
        assert!(dropped.released);
        assert!(get(&conn, "bank").expect("get").is_none(), "gone");
    }

    #[test]
    fn bad_input_is_refused() {
        let (_dir, conn) = store();
        assert!(acquire(&conn, "  ", "hand-a", None).is_err());
        assert!(acquire(&conn, "bank", "", None).is_err());
        assert!(acquire(&conn, "bank", "hand-a", Some(0)).is_err());
        assert!(acquire(&conn, "bank", "hand-a", Some(MAX_TTL_SECONDS + 1)).is_err());
    }

    #[test]
    fn list_shows_expired_rows() {
        let (_dir, conn) = store();
        acquire(&conn, "bank", "hand-a", Some(1)).expect("first");
        conn.execute(
            "UPDATE claim SET expires_at = datetime('now', '-1 second') WHERE name = 'bank'",
            [],
        )
        .expect("expire");
        let rows = list(&conn).expect("list");
        assert_eq!(rows.len(), 1, "an expired claim is kept, not swept");
        assert!(!rows[0].live, "and reads as not live");
    }
}
