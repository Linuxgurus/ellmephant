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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
    /// `policy` (borrowed under a `sudoers` row) or `lend` (a pen lending its own authority down).
    pub source: String,
}

/// The largest duration any single session may be granted, in seconds (24 h).
///
/// Not a policy; a ceiling on policies. A `sudo` whose whole point is "cluster-wide hygiene once"
/// does not need a week, and a week-long session is a credential.
pub const MAX_DURATION_SECS: i64 = 86_400;

fn id_of(conn: &Connection, name: &str) -> Result<i64> {
    // Folds case like every other lookup (migration 0027): `james` and `James` are one pen.
    conn.query_row(
        "SELECT id FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| r.get(0),
    )
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

/// **LEND: a pen hands its own authority DOWN to a descendant** (James, 2026-10-23).
///
/// The mirror of [`sudo`], and the reason they are different verbs: `sudo` borrows somebody else's
/// authority ACROSS or UPWARD and therefore needs a `sudoers` row; lending spends authority you
/// already have, and parentage already grants that. Requiring a policy row to lend downward would
/// mean every minted finger needs a configuration entry — configuration written by code.
///
/// ★ **TWO DIRECTIONS, TWO KINDS OF RISK** (James agreeing with Stet, 2026-10-23). Lending DOWN is a
/// self extending itself into a mechanism: the finger acts with the hand's authority by construction,
/// so the exposure is STRUCTURAL and a session bounds TIME, not trust — there is no third party to
/// check a hand on its own work. Borrowing ACROSS or UP ([`sudo`]) is the other side: a pen reaching
/// for authority it does not have, checked by policy and by someone else's interest. A session
/// narrows both, and "solved" is the wrong word for either — but only on the borrowing side is the
/// bound also a CHECK.
///
/// What lending keeps is the part that matters: it EXPIRES, it can be REVOKED by deleting one row,
/// and `via` lets a lend's children die with it.
///
/// ★ **A lend SETS the deadline; it does not stack one on top.** Two live rows for the same pair
/// would mean the effective deadline is whichever happens to be later, and a SHORTER lend would
/// reappear as the longer one after it lapsed. So lending again replaces the pair's sessions (and
/// everything passed down from them). To end one early, `release`. Without this, a finger's authority would be a fact
/// about tree shape and nothing could revoke it short of re-shaping the tree.
pub fn lend(conn: &Connection, powerful: &str, weak: &str, secs: i64) -> Result<Session> {
    if powerful.eq_ignore_ascii_case(weak) {
        return Err(Error::Invalid(format!(
            "{powerful} cannot lend to itself: its own authority is not borrowed, it is had"
        )));
    }
    let (p, _, ceased) = pen_identity(conn, powerful)?;
    if ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{powerful} has been retired: a pen that has ceased lends nothing"
        )));
    }
    // (j) A lend to a pen that has CEASED is a lend to nobody, so it is refused here rather than left
    // to a later check that might not exist on every path.
    let (w, _, ceased) = pen_identity(conn, weak)?;
    if ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{weak} has been retired: authority lent to a pen that has ceased is authority lent to \
             nobody"
        )));
    }
    if !is_below(conn, p, w)? {
        return Err(Error::Invalid(format!(
            "{weak} is not a descendant of {powerful}: authority is lent DOWN, never sideways or up"
        )));
    }
    if !(1..=MAX_DURATION_SECS).contains(&secs) {
        return Err(Error::Invalid(format!(
            "a lend must last between 1 and {MAX_DURATION_SECS} seconds (asked for {secs})"
        )));
    }
    // ★ A LEND REPLACES WHAT WAS LENT (Stet, review of #126). Two live rows for one pair are not a
    // tightening and not a renewal — they are two deadlines, and the later one wins by accident.
    // Worse, the SHORTER of them expiring reveals the longer one again: a lend you shortened comes
    // back on its own. So a lend is a SET: the pair's live sessions end (with everything passed down
    // from them, or a child would outlive the grant it was passed under), then the new one begins.
    // To shorten without re-lending is `release`; to extend is another `lend`.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "WITH RECURSIVE chain(id) AS (
             SELECT id FROM active_sudo WHERE weak = ?1 AND powerful = ?2
             UNION
             SELECT child.id FROM active_sudo child JOIN chain ON child.via = chain.id
         )
         DELETE FROM active_sudo WHERE id IN (SELECT id FROM chain)",
        params![w, p],
    )?;
    tx.execute(
        "INSERT INTO active_sudo (powerful, weak, expires_at, source)
         VALUES (?1, ?2, datetime('now', ?3), 'lend')",
        params![p, w, format!("+{secs} seconds")],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
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
        source: "lend".to_string(),
    })
}

/// `(id, parent_id, ceased_at)` for a pen, or a refusal that names it.
fn pen_identity(conn: &Connection, name: &str) -> Result<(i64, Option<i64>, Option<String>)> {
    conn.query_row(
        "SELECT id, parent_id, ceased_at FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

/// Borrow `powerful`'s authority as `weak` for `secs`. A session, not a permission: it expires.
pub fn sudo(conn: &Connection, weak: &str, powerful: &str, secs: i64) -> Result<Session> {
    if weak == powerful {
        return Err(Error::Invalid(format!("{weak} cannot sudo to itself")));
    }
    // ★ A CEASED PEN HAS NO AUTHORITY TO LEND OR BORROW (found reviewing #127). `retire` now ends
    // live sessions, but a retired pen's `sudoers` row would otherwise stay borrowable forever, and a
    // retired pen could still ask for someone else's. Both are refused where the request is made.
    let (p, _, powerful_ceased) = pen_identity(conn, powerful)?;
    if powerful_ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{powerful} has been retired: a pen that has ceased lends no authority"
        )));
    }
    let (w, _, weak_ceased) = pen_identity(conn, weak)?;
    if weak_ceased.is_some() {
        return Err(Error::Invalid(format!(
            "{weak} has been retired: a pen that has ceased borrows nothing"
        )));
    }
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
    // ★ BORROWING REPLACES TOO (Stet's catch on #126, applied to the other verb). A second `sudo`
    // for the same pair is not a renewal on top of the old row: two live rows would mean the later
    // deadline wins by accident, and a SHORTENED borrow would come back when the short one lapsed.
    // So: end the pair's live sessions (cascading through `via`), then begin the new one.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "WITH RECURSIVE chain(id) AS (
             SELECT id FROM active_sudo WHERE weak = ?1 AND powerful = ?2
             UNION
             SELECT child.id FROM active_sudo child JOIN chain ON child.via = chain.id
         )
         DELETE FROM active_sudo WHERE id IN (SELECT id FROM chain)",
        params![w, p],
    )?;
    tx.execute(
        "INSERT INTO active_sudo (powerful, weak, expires_at, source)
         VALUES (?1, ?2, datetime('now', ?3), 'policy')",
        params![p, w, format!("+{secs} seconds")],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
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
        source: "policy".to_string(),
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
        "INSERT INTO active_sudo (powerful, weak, expires_at, via, source)
         VALUES ((SELECT id FROM pen WHERE lower(name) = lower(?1)), ?2, datetime('now', ?3), ?4, ?5)",
        params![
            powerful,
            to_id,
            format!("+{secs} seconds"),
            parent.id,
            parent.source
        ],
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
        // A passed session carries the authority it was passed from: a lend stays a lend, a borrow
        // stays a borrow, however far down the chain it travels.
        source: parent.source,
    })
}

/// Is `descendant` below `ancestor` in the pen tree? (A pen is not its own descendant here.)
pub(crate) fn is_below(conn: &Connection, ancestor: i64, descendant: i64) -> Result<bool> {
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
            "SELECT id, expires_at, via, source FROM active_sudo
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
                    source: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// End one session and everything passed down from it. **`caller` must be the pen itself or an
/// ancestor of it** — the pen that lent authority must be able to stop what it lent, or "a pen that
/// goes bad at 03:00" is stoppable only by the pen that went bad.
pub fn release_by(conn: &Connection, caller: &str, weak: &str, powerful: &str) -> Result<usize> {
    let caller_id = id_of(conn, caller)?;
    let weak_id = id_of(conn, weak)?;
    if caller_id != weak_id && !is_below(conn, caller_id, weak_id)? {
        return Err(Error::Invalid(format!(
            "{caller} may not end {weak}'s session: only that pen or one of its ancestors may"
        )));
    }
    release(conn, weak, powerful)
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
        "SELECT s.id, p.name, s.expires_at, s.via, s.source
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
            source: r.get(4)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// ★ WHOSE AUTHORITY THIS PEN MAY SPEND **NOW** (adversarial review, 2026-10-24).
///
/// `held()` answers "what sessions exist". This answers the harder question: **do they still stand for
/// anything?** A `lend` is granted only because the lender is an ANCESTOR of the borrower — so if the
/// tree moves underneath it (an adoption, a disclaim, a repair), the session would keep handing out
/// reach that no longer has a reason. A session that outlives its justification is a credential.
///
/// A `policy` borrow is different: the policy row IS the grant, and a `sudoers` ceiling does not depend
/// on the tree, so it stands until revoked or expired.
///
/// Returns the powerful pens' ids for the sessions that still hold water, plus the NAME and source so a
/// caller can record what it spent.
pub fn reachable(conn: &Connection, weak: &str) -> Result<Vec<(i64, String, String)>> {
    let mut out = Vec::new();
    for s in held(conn, weak)? {
        let (pid, _, _) = match pen_identity(conn, &s.powerful) {
            Ok(v) => v,
            Err(_) => continue, // a session naming a pen that no longer exists grants nothing
        };
        let wid = match pen_identity(conn, weak) {
            Ok((id, _, _)) => id,
            Err(_) => return Ok(out),
        };
        if s.source == "lend" && !is_below(conn, pid, wid)? {
            tracing::warn!(
                weak = %weak,
                powerful = %s.powerful,
                "a lend outlived the parentage that justified it: standing down (revoke it)"
            );
            continue;
        }
        out.push((pid, s.powerful.clone(), s.source.clone()));
    }
    Ok(out)
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
