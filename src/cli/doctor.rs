//! `ellmephant doctor` — paths, schema version and the health of every *derived* index
//! (links, tags, vectors), plus `--verify` for a full re-hash of the weights (PLAN §6 P9).

use rusqlite::Connection;

use super::format::human_bytes;
use super::model::verify_weights;
use crate::config::{Config, Paths};
use crate::error::Result;
use crate::model;
use crate::store::{links, project, tags};
use crate::{db, index};

/// Report paths, schema version and derived-index health; `verify` re-hashes the weights.
pub fn run(paths: &Paths, cfg: &Config, spec: &model::Spec, verify: bool) -> Result<()> {
    paths.ensure()?;
    let conn = db::open(paths)?;
    let schema = db::migrate::version(&conn)?;
    let objects: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view','trigger')",
        [],
        |r| r.get(0),
    )?;
    print_env(paths, cfg, schema, objects);
    print_weights(spec)?;
    print_index_health(&conn)?;
    if verify {
        verify_weights(spec, true)?;
    }
    Ok(())
}

/// Paths, schema version and the configured model (PLAN §6 P9).
fn print_env(paths: &Paths, cfg: &Config, schema: i32, objects: i64) {
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
}

/// The weights-cache summary (no network; SPEC §6.5).
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

/// Derived-index health (PLAN §6 P9): rows, `vec0`, links and tags.
fn print_index_health(conn: &Connection) -> Result<()> {
    let memories: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
    // The vec0 table only exists once something has been written; `stored_dim` is the tell.
    let dim = index::vec::stored_dim(conn)?;
    let vectors = match dim {
        Some(_) => index::vec::count(conn)?,
        None => 0,
    };
    let vec_status = match dim {
        Some(dim) => format!("vec0 dim={dim}, {vectors}/{memories} vectors"),
        None => format!("vec0 dim=? ({vectors}/{memories} vectors)"),
    };
    println!("  vec0           {vec_status}");

    // The embed projection is fingerprinted in `meta`; a mismatch means the stored vectors were
    // built from a different document than the code now emits — a `reembed` job, not corruption.
    let projection = if index::vec::projection_stale(conn)? {
        "STALE — run `reembed`".to_string()
    } else if db::meta::get(conn, db::meta::PROJECTION)?.is_some() {
        "current".to_string()
    } else {
        "unrecorded (reembed to record)".to_string()
    };
    println!("  projection     {projection}");

    // Derived-index health (PLAN §6 P9): a dangling `[[key]]` is a lost rename target, a
    // missing vector is a `reembed` job.
    let edges: i64 = conn.query_row("SELECT count(*) FROM memory_link", [], |r| r.get(0))?;
    let dangling = links::dangling(conn)?.len();
    println!("  links          {edges} edge(s), {dangling} dangling");
    let tag_count = tags::list(conn)?.len();
    println!("  tags           {tag_count} tag(s)");
    // Projects are a second table, not a second graph: report them on their own line, with the
    // project-side rot (`preload` keys that resolve to no memory) alongside it.
    let projects = project::count(conn)?;
    let project_rot = project::dangling(conn)?.len();
    println!("  projects       {projects} project(s), {project_rot} dangling preload key(s)");
    println!(
        "  vectors        {} row(s) without a vector",
        (memories - vectors).max(0)
    );
    Ok(())
}
