use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const DEFAULT_TTL_SECONDS: i64 = 900;

pub const MAX_TTL_SECONDS: i64 = 86_400;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub name: String,
    pub owner: String,
    pub acquired_at: String,
    pub expires_at: String,
    pub live: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Acquire {
    pub won: bool,
    pub claim: Claim,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub released: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

const COLUMNS: &str = "name, owner, acquired_at, expires_at, (expires_at > datetime('now'))";

fn row_to_claim(r: &rusqlite::Row<'_>) -> rusqlite::Result<Claim> {
    Ok(Claim {
        name: r.get(0)?,
        owner: r.get(1)?,
        acquired_at: r.get(2)?,
        expires_at: r.get(3)?,
        live: r.get(4)?,
    })
}

fn normalize_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(Error::Claim("a claim needs a non-empty `name`".to_string()));
    }
    Ok(name.to_lowercase())
}

fn normalize_owner(raw: &str) -> Result<String> {
    let owner = raw.trim();
    if owner.is_empty() {
        return Err(Error::Claim(
            "a claim needs a non-empty `owner`".to_string(),
        ));
    }
    Ok(owner.to_lowercase())
}

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
    match get(conn, &name)? {
        Some(claim) => Ok(Acquire { won: false, claim }),
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

pub fn release(conn: &Connection, name: &str, owner: &str) -> Result<Release> {
    release_by(conn, name, owner, None)
}

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
    if ancestor && dropped > 0 {
        conn.execute(
            "INSERT INTO claim_release_log (name, owner, released_by, reason) VALUES (?1, ?2, ?3, ?4)",
            params![name, claim.owner, actor, stated.unwrap_or_default()],
        )?;
    }
    let dropped_now = dropped > 0;
    Ok(Release {
        released: dropped_now,
        claim: if dropped_now { None } else { get(conn, &name)? },
        released_by: if ancestor && dropped_now {
            Some(actor.clone())
        } else {
            None
        },
        reason: if ancestor && dropped_now {
            stated.map(str::to_string)
        } else {
            None
        },
    })
}

fn pen_id(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM pen WHERE lower(name) = lower(?1)",
            params![name],
            |r| r.get(0),
        )
        .optional()?)
}

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

pub type Revocation = (String, String, String, String, String);

pub fn revocations(conn: &Connection, name: Option<&str>, limit: i64) -> Result<Vec<Revocation>> {
    let mut stmt = match name {
        Some(n) => {
            let mut st = conn.prepare(
                "SELECT at, name, owner, released_by, reason FROM claim_release_log
                 WHERE lower(name) = lower(?1) ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = st.query_map(params![n.trim(), limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            return rows
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into);
        }
        None => conn.prepare(
            "SELECT at, name, owner, released_by, reason FROM claim_release_log
             ORDER BY id DESC LIMIT ?1",
        )?,
    };
    let rows = stmt.query_map(params![limit], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

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
        let probe = acquire(&conn, "finger", "hands", Some(60)).expect("probe");
        assert!(probe.won, "the holder may renew");
        assert_eq!(
            probe.claim.expires_at, long.claim.expires_at,
            "a shorter TTL must not move the deadline backwards"
        );
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
        conn.execute(
            "UPDATE claim SET expires_at = datetime('now', '-1 second') WHERE name = 'bank'",
            [],
        )
        .expect("expire");
        let out = acquire(&conn, "bank", "hand-b", Some(300)).expect("second");
        assert!(out.won, "an expired lease must not block");
        assert_eq!(out.claim.owner, "hand-b");
    }

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
    fn an_ancestor_release_is_on_the_record() {
        let (_dir, conn) = store();
        pens(&conn);
        acquire(&conn, "finger", "hands", Some(300)).expect("acquire");
        release_by(&conn, "finger", "stet", Some("the launcher was killed"))
            .expect("ancestor release");
        let rows = revocations(&conn, Some("finger"), 10).expect("revocations");
        assert_eq!(rows.len(), 1, "one act, one row");
        let (_, name, owner, by, reason) = &rows[0];
        assert_eq!(name, "finger");
        assert_eq!(owner, "hands", "who held it");
        assert_eq!(by, "stet", "who ended it");
        assert_eq!(reason, "the launcher was killed");
    }

    #[test]
    fn a_release_that_dropped_nothing_claims_nothing() {
        let (_dir, conn) = store();
        pens(&conn);
        let out = release_by(&conn, "finger", "stet", Some("raced")).expect("no lease");
        assert!(!out.released);
        assert!(
            out.released_by.is_none(),
            "it must not report releasing what is still there"
        );
        assert!(out.reason.is_none());
        assert!(
            revocations(&conn, Some("finger"), 10)
                .expect("revocations")
                .is_empty()
        );
    }

    #[test]
    fn a_holders_own_release_writes_nothing() {
        let (_dir, conn) = store();
        pens(&conn);
        acquire(&conn, "finger", "hands", Some(300)).expect("acquire");
        release_by(&conn, "finger", "hands", None).expect("own release");
        assert!(
            revocations(&conn, Some("finger"), 10)
                .expect("revocations")
                .is_empty(),
            "the tidy path is not an act on someone else's lease"
        );
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
