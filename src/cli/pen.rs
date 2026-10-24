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
