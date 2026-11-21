use crate::config::{Config, Paths};
use crate::db;
use crate::error::{Error, Result};
use crate::store;

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

pub fn rotate(paths: &Paths, cfg: &Config, name: &str) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let secret = store::credential::rotate(&conn, None, name)?;
    println!("  rotated   {name}");
    println!("  secret    {secret}");
    println!("            shown ONCE — the previous secret is dead on the next call.");
    Ok(())
}

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

use crate::store::sudo;

pub fn grant(paths: &Paths, cfg: &Config, powerful: &str, weak: &str, secs: i64) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    sudo::grant(&conn, powerful, weak, secs, "cli")?;
    println!("  policy    {weak} may borrow {powerful} for at most {secs}s");
    Ok(())
}

pub fn lend(paths: &Paths, cfg: &Config, powerful: &str, weak: &str, secs: i64) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let session = sudo::lend(&conn, powerful, weak, secs)?;
    report(&session);
    Ok(())
}

pub fn borrow(paths: &Paths, cfg: &Config, weak: &str, powerful: &str, secs: i64) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let session = sudo::sudo(&conn, weak, powerful, secs)?;
    report(&session);
    Ok(())
}

pub fn release(paths: &Paths, cfg: &Config, weak: &str, powerful: &str) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let n = sudo::release(&conn, weak, powerful)?;
    println!("  released  {n} session(s): {weak} no longer holds {powerful}");
    Ok(())
}

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

pub fn revoke(paths: &Paths, cfg: &Config, powerful: &str, weak: &str) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let killed = sudo::revoke(&conn, powerful, weak)?;
    println!("  revoked   {weak} may no longer borrow {powerful}; {killed} live session(s) ended");
    Ok(())
}

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
