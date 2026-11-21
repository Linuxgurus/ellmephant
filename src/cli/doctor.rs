use rusqlite::Connection;

use super::format::human_bytes;
use super::model::verify_weights;
use crate::config::{Config, Paths};
use crate::error::Result;
use crate::model;
use crate::store::{links, maintain, project, tags, version};
use crate::{db, index};

pub fn run(paths: &Paths, cfg: &Config, spec: &model::Spec, verify: bool) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let schema = db::migrate::version(&conn)?;
    let objects: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view','trigger')",
        [],
        |r| r.get(0),
    )?;
    print_env(paths, cfg, schema, objects, &conn);
    print_weights(spec)?;
    print_index_health(&conn)?;
    if verify {
        verify_weights(spec, true)?;
    }
    Ok(())
}

pub fn reparse(paths: &Paths, cfg: &Config) -> Result<()> {
    paths.ensure()?;
    let conn = db::open_with(paths, cfg)?;
    let report = maintain::reparse(&conn)?;
    println!(
        "re-parsed {} document(s); the graph now holds {} edge(s)",
        report.rows, report.edges
    );
    let dangling = links::dangling(&conn)?;
    println!("  dangling targets  {}", dangling.len());
    Ok(())
}

fn print_env(paths: &Paths, cfg: &Config, schema: i32, objects: i64, conn: &Connection) {
    println!("ellmephant {}", crate::VERSION);
    println!("  root           {}", paths.root().display());
    println!("  db             {}", paths.db().display());
    println!(
        "  config         {} ({})",
        paths.config().display(),
        if paths.config().exists() {
            "file"
        } else {
            "defaults"
        }
    );
    println!("  models         {}", paths.models().display());
    println!(
        "  schema         v{} (binary knows v{})",
        schema,
        db::migrate::latest()
    );
    println!("  objects        {objects}");
    println!("  model          {} dim={}", cfg.model, cfg.dim);
    let tree = (schema >= 21)
        .then(|| {
            conn.query_row(
                "SELECT count(*), sum(CASE WHEN parent_id IS NULL THEN 1 ELSE 0 END) FROM pen",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    ))
                },
            )
            .ok()
        })
        .flatten();
    if let Some((pens, roots)) = tree {
        let note = if roots > 1 {
            "   <- NOT ATTACHED: seeded names, no parents; ancestry reaches nothing"
        } else {
            ""
        };
        println!("  pens           {pens} pen(s), {roots} root(s){note}");
    }
}

fn print_weights(spec: &model::Spec) -> Result<()> {
    let weights = model::status(spec)?;
    let present = weights.files.iter().filter(|f| f.present).count();
    let bytes: u64 = weights.files.iter().filter_map(|f| f.size).sum();
    let at = match &weights.resolved {
        Some(sha) => format!("{}@{}", weights.repo, &sha[..sha.len().min(12)]),
        None => format!("{}@{}", weights.repo, weights.revision),
    };
    println!(
        "  weights        {}  {present}/{} files, {}{}",
        at,
        weights.files.len(),
        human_bytes(bytes),
        if weights.offline { " (offline)" } else { "" }
    );
    Ok(())
}

fn print_index_health(conn: &Connection) -> Result<()> {
    let memories: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
    let (commons, unattributed): (i64, i64) = conn.query_row(
        "SELECT sum(CASE WHEN lower(trim(asserted_by)) NOT IN ('', 'anonymous') THEN 1 ELSE 0 END),
                sum(CASE WHEN lower(trim(asserted_by)) IN ('', 'anonymous') THEN 1 ELSE 0 END)
           FROM memory WHERE owner IS NULL",
        [],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            ))
        },
    )?;
    println!("  commons        {commons} row(s) released to nobody, author still named");
    println!(
        "  unattributed   {unattributed} row(s) no currently existing pen wrote — no owner, no author; claim them"
    );
    let dim = index::vec::stored_dim(conn)?;
    let vectors = match dim {
        Some(_) => index::vec::count(conn)?,
        None => 0,
    };
    let per_type = if dim.is_some() {
        let mut parts: Vec<String> = Vec::new();
        let mem = index::vec::count_of(conn, "memory")?;
        parts.push(format!("memory {mem}/{memories}"));
        let deleted = index::vec::count_deleted(conn)?;
        if deleted > 0 {
            parts.push(format!("tombstoned {deleted}"));
        }
        parts.join(", ")
    } else {
        format!("memory 0/{memories}")
    };
    let vec_status = match dim {
        Some(dim) => format!("vec0 dim={dim}, {vectors} vector(s) — {per_type}"),
        None => format!("vec0 dim=? (no index yet) — {per_type}"),
    };
    println!("  vec0           {vec_status}");

    let memories: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
    let verdict = crate::store::encode::projection(conn)?;
    println!("  projection     {}", verdict.verdict(vectors, memories));
    if let Some(record) = db::meta::get(conn, maintain::REBUILD)? {
        println!("  rebuild        {record}");
    }

    let edges: i64 = conn.query_row("SELECT count(*) FROM memory_link", [], |r| r.get(0))?;
    let dangling = links::dangling(conn)?.len();
    println!("  links          {edges} edge(s), {dangling} dangling");
    let tag_count = tags::list(conn)?.len();
    println!("  tags           {tag_count} tag(s)");
    let projects = project::count(conn)?;
    let project_rot = project::dangling(conn)?.len();
    println!("  projects       {projects} project(s), {project_rot} dangling preload key(s)");
    let (versions, version_bytes) = version::count(conn)?;
    let version_orphans = version::index(conn)?.iter().filter(|i| i.orphan).count();
    println!(
        "  versions       {versions} row(s), {}, {version_orphans} of a deleted key",
        human_bytes(version_bytes as u64)
    );
    println!(
        "  vectors        {} row(s) without a vector",
        (memories - vectors).max(0)
    );
    Ok(())
}
