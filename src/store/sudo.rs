use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Session {
    pub id: i64,
    pub powerful: String,
    pub weak: String,
    pub expires_at: String,
    pub via: Option<i64>,
    pub source: String,
}

pub const MAX_DURATION_SECS: i64 = 86_400;

fn id_of(conn: &Connection, name: &str) -> Result<i64> {
    conn.query_row(
        "SELECT id FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

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

fn pen_identity(conn: &Connection, name: &str) -> Result<(i64, Option<i64>, Option<String>)> {
    conn.query_row(
        "SELECT id, parent_id, ceased_at FROM pen WHERE lower(name) = lower(?1)",
        params![name],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()?
    .ok_or_else(|| Error::Invalid(format!("no pen named {name}")))
}

pub fn sudo(conn: &Connection, weak: &str, powerful: &str, secs: i64) -> Result<Session> {
    if weak == powerful {
        return Err(Error::Invalid(format!("{weak} cannot sudo to itself")));
    }
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
        source: parent.source,
    })
}

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

pub fn reachable(conn: &Connection, weak: &str) -> Result<Vec<(i64, String, String)>> {
    let mut out = Vec::new();
    for s in held(conn, weak)? {
        let (pid, _, _) = match pen_identity(conn, &s.powerful) {
            Ok(v) => v,
            Err(_) => continue,
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

pub fn live_sessions(conn: &Connection, weak: Option<&str>) -> Result<Vec<Session>> {
    if let Some(w) = weak {
        return held(conn, w);
    }
    let mut stmt = conn.prepare(
        "SELECT s.id, p.name, w.name, s.expires_at, s.via, s.source
           FROM active_sudo s
           JOIN pen p ON p.id = s.powerful
           JOIN pen w ON w.id = s.weak
          WHERE s.expires_at > datetime('now')
          ORDER BY s.expires_at",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Session {
            id: r.get(0)?,
            powerful: r.get(1)?,
            weak: r.get(2)?,
            expires_at: r.get(3)?,
            via: r.get(4)?,
            source: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

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

pub const BOOT_ENV: &str = "ELLMEPHANT_SUDOERS";

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
