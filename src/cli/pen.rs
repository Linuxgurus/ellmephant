//! `ellmephant pen …` — the PROVISIONING door: standing, on purpose (2026-10-24).
//!
//! The runtime door (`pen_mint` over MCP) lets a live pen create a pen under *itself*, with the caller's
//! authority behind it. Provisioning is the other door: whoever can open the database says what a pen IS —
//! its parent, and that it exists at all. Until now that door was "somebody runs SQL", which nothing could
//! do (no `sqlite3` in the image, no mounting the PVC beside a running brain) — so the attach had to be
//! smuggled in as boot configuration, and James called it: *"the estate is in an environment variable
//! instead of a table?"*
//!
//! It is not, and this is how it stops being one. The env var is a **seed** — it only fills a `parent_id`
//! that is empty. Moving a pen that already has a standing is a deliberate act, and deliberate acts have a
//! verb, a shell history entry and a log line:
//!
//! ```console
//! $ ellmephant pen attach 'stet=estate' 'twine=stet'
//! $ ellmephant pen tree
//! ```
//!
//! Nothing here is new authority: `set_parent` and `mint` already refuse a loop, a retired pen, an
//! unaddressable name and the reserved word. This is only the way to reach them.

use crate::config::{Config, Paths};
use crate::db;
use crate::error::{Error, Result};
use crate::store;

/// `pen attach child=parent …` — move pens, deliberately. Overwrites what the table says.
pub fn attach(paths: &Paths, cfg: &Config, edges: &[String]) -> Result<()> {
    if edges.is_empty() {
        return Err(Error::Invalid(
            "pen attach wants at least one `child=parent` edge".into(),
        ));
    }
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let mut changed = 0;
    let mut already = 0;
    // Report every failure and still apply the rest, then FAIL the command if any was refused: this is a
    // provisioning shell, and a typo must not look like success.
    let mut refusals: Vec<String> = Vec::new();
    for edge in edges {
        let Some((child, parent)) = edge.split_once('=') else {
            refusals.push(format!("{edge}: expected `child=parent`"));
            continue;
        };
        let (child, parent) = (child.trim(), parent.trim());
        match store::seed_parent(&conn, child, parent) {
            Ok(store::Seeded::Attached) => {
                println!("  attached  {child} → {parent}");
                changed += 1;
            }
            Ok(store::Seeded::Already) => {
                println!("  already   {child} → {parent}");
                already += 1;
            }
            Ok(store::Seeded::Disagrees(current)) => {
                // ★ THE TABLE WINS TO THE SEED, NOT TO YOU. A verb typed on purpose overwrites.
                store::set_parent(&conn, child, parent)?;
                println!("  moved     {child}: {current} → {parent}");
                changed += 1;
            }
            Err(e) => refusals.push(format!("{child}={parent}: {e}")),
        }
    }
    for r in &refusals {
        eprintln!("  refused   {r}");
    }
    println!(
        "{changed} changed, {already} already stood, {} refused",
        refusals.len()
    );
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{} edge(s) refused",
            refusals.len()
        )))
    }
}

/// `pen mint <name> --parent <parent> [--kind self|finger]` — create a pen by provisioning.
///
/// The runtime door wants a live caller with authority; this one wants the database. It refuses exactly
/// what `store::mint` refuses — `anonymous`, a name that cannot be addressed, a retired parent — because
/// a provisioning door is not a licence to make a broken row.
pub fn mint(paths: &Paths, cfg: &Config, name: &str, parent: &str, kind: &str) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let minted = store::mint(&conn, parent, name, kind)?;
    println!(
        "  minted    {} ({}) under {}",
        minted.name, minted.kind, minted.parent
    );
    println!("  secret    {}", minted.secret);
    println!("            shown ONCE — hand it to the pen, then have it rotate.");
    Ok(())
}

/// `pen rotate <name>` — provisioning: replace a credential and print the new secret once.
///
/// Provisioning, not a door: whoever can open the database can already read every hash, so there is
/// no caller to authorize against. The runtime door (`pen_rotate` over MCP) is the one that checks
/// self-or-descendant.
pub fn rotate(paths: &Paths, cfg: &Config, name: &str) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let secret = store::credential::rotate(&conn, None, name)?;
    println!("  rotated   {name}");
    println!("  secret    {secret}");
    println!("            shown ONCE — the previous secret is dead on the next call.");
    Ok(())
}

/// `pen tree` — the shape, and the one number that matters: the number of roots.
pub fn tree(paths: &Paths, cfg: &Config) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let mut stmt = conn.prepare(
        "SELECT p.name, coalesce(q.name, ''), coalesce(p.ceased_at, '')
           FROM pen p LEFT JOIN pen q ON q.id = p.parent_id
          ORDER BY p.name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut roots = 0;
    for row in rows {
        let (name, parent, ceased) = row?;
        let tomb = if ceased.is_empty() { "" } else { "  (retired)" };
        if parent.is_empty() {
            roots += 1;
            println!("  {name:<12} (root){tomb}");
        } else {
            println!("  {name:<12} ← {parent}{tomb}");
        }
    }
    println!("{roots} root(s) — one root is the invariant");
    Ok(())
}

// ── AUTHORITY, BY HAND (2026-10-24) ──────────────────────────────────────────────────────────────
// These are the *provisioning* half of the sudo model: the policy, the lend, the borrow. They exist
// because `kubectl exec` is already a privilege that owns the box — so a verb here grants nothing that
// the person typing it could not already do with the database open. What they do NOT do is decide
// whether a *live pen* may invoke them: that is the door's question, and the door is not built.

use crate::store::sudo;

/// `pen grant <powerful> <weak> <secs>` — a sudoers policy: weak may borrow powerful for at most secs.
pub fn grant(paths: &Paths, cfg: &Config, powerful: &str, weak: &str, secs: i64) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    sudo::grant(&conn, powerful, weak, secs, "cli")?;
    println!("  policy    {weak} may borrow {powerful} for at most {secs}s");
    Ok(())
}

/// `pen lend <powerful> <weak> <secs>` — powerful lends its own authority DOWN to weak.
pub fn lend(paths: &Paths, cfg: &Config, powerful: &str, weak: &str, secs: i64) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let session = sudo::lend(&conn, powerful, weak, secs)?;
    report(&session);
    Ok(())
}

/// `pen sudo <weak> <powerful> <secs>` — weak borrows powerful, under a policy.
pub fn borrow(paths: &Paths, cfg: &Config, weak: &str, powerful: &str, secs: i64) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let session = sudo::sudo(&conn, weak, powerful, secs)?;
    report(&session);
    Ok(())
}

/// `pen release <weak> <powerful>` — end a session early.
pub fn release(paths: &Paths, cfg: &Config, weak: &str, powerful: &str) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let n = sudo::release(&conn, weak, powerful)?;
    println!("  released  {n} session(s): {weak} no longer holds {powerful}");
    Ok(())
}

/// `pen sessions [<weak>]` — who holds what, and until when.
pub fn sessions(paths: &Paths, cfg: &Config, weak: Option<&str>) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let held: Vec<sudo::Session> = match weak {
        Some(w) => sudo::held(&conn, w)?,
        None => {
            let mut stmt = conn.prepare(
                "SELECT s.id, p.name, w.name, s.expires_at, s.via, s.source
                   FROM active_sudo s
                   JOIN pen p ON p.id = s.powerful
                   JOIN pen w ON w.id = s.weak
                  WHERE s.expires_at > datetime('now')
                  ORDER BY s.expires_at",
            )?;
            stmt.query_map([], |r| {
                Ok(sudo::Session {
                    id: r.get(0)?,
                    powerful: r.get(1)?,
                    weak: r.get(2)?,
                    expires_at: r.get(3)?,
                    via: r.get(4)?,
                    source: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?
        }
    };
    if held.is_empty() {
        println!("  no live sessions");
        return Ok(());
    }
    for s in held {
        let via = s.via.map(|v| format!(" via #{v}")).unwrap_or_default();
        println!(
            "  {:<10} holds {:<10} ({:<6}) until {}{via}",
            s.weak, s.powerful, s.source, s.expires_at
        );
    }
    Ok(())
}

fn report(session: &sudo::Session) {
    println!(
        "  {:<9} {} holds {} ({}) until {}",
        "session", session.weak, session.powerful, session.source, session.expires_at
    );
}

/// `pen revoke <powerful> <weak>` — remove a policy AND every session it authorised, in one act.
///
/// A policy removed while its sessions keep running is a revocation that has not happened yet, and the
/// sessions would be indistinguishable from permission.
pub fn revoke(paths: &Paths, cfg: &Config, powerful: &str, weak: &str) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let killed = sudo::revoke(&conn, powerful, weak)?;
    println!("  revoked   {weak} may no longer borrow {powerful}; {killed} live session(s) ended");
    Ok(())
}

/// `pen policies` — every standing grant, and where it came from.
pub fn policies(paths: &Paths, cfg: &Config) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let mut stmt = conn.prepare(
        "SELECT p.name, w.name, s.max_duration_secs, s.source, s.granted_at
           FROM sudoers s
           JOIN pen p ON p.id = s.powerful
           JOIN pen w ON w.id = s.weak
          ORDER BY w.name COLLATE NOCASE, p.name COLLATE NOCASE",
    )?;
    let rows: Vec<(String, String, i64, String, String)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        println!("  no standing grants");
        return Ok(());
    }
    for (powerful, weak, secs, source, at) in rows {
        println!("  {weak:<10} may borrow {powerful:<10} for at most {secs}s  ({source}, {at})");
    }
    Ok(())
}
