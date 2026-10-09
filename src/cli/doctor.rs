//! `llmphant doctor` — paths, schema version and the health of every *derived* index
//! (links, tags, vectors), plus `--verify` for a full re-hash of the weights (PLAN §6 P9).

use super::format::human_bytes;
use crate::config::{Config, Paths};
use crate::error::{Error, Result};
use crate::model;
use crate::store::{links, tags};
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

    println!("llmphant {}", crate::VERSION);
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

    // Weights cache summary (no network; SPEC §6.5).
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

    let memories: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
    // `memory_vec` only exists once something has been written; `stored_dim` is the tell.
    let dim = index::vec::stored_dim(&conn)?;
    let vectors = match dim {
        Some(_) => index::vec::count(&conn)?,
        None => 0,
    };
    let vec_status = match dim {
        Some(dim) => format!("vec0 dim={dim}, {vectors}/{memories} rows embedded"),
        None => format!("vec0 dim=? ({vectors}/{memories} rows embedded)"),
    };
    println!("  vec0           {vec_status}");

    // Derived-index health (PLAN §6 P9): a dangling `[[key]]` is a lost rename target, an
    // orphan tag is vocabulary a delete left behind, a missing vector is a `reembed` job.
    let edges: i64 = conn.query_row("SELECT count(*) FROM memory_link", [], |r| r.get(0))?;
    let dangling = links::dangling(&conn)?.len();
    println!("  links          {edges} edge(s), {dangling} dangling");
    let tag_count: i64 = conn.query_row("SELECT count(*) FROM tag", [], |r| r.get(0))?;
    let orphans = tags::orphans(&conn)?.len();
    println!("  tags           {tag_count} tag(s), {orphans} orphaned");
    println!(
        "  vectors        {} row(s) without a vector",
        (memories - vectors).max(0)
    );

    if verify {
        match model::verify(spec) {
            Ok(bad) if bad.is_empty() => println!("  verify         ok (all sha256 match)"),
            Ok(bad) => {
                for line in &bad {
                    println!("  verify         MISMATCH {line}");
                }
                return Err(Error::Model(format!(
                    "{} file(s) failed verification",
                    bad.len()
                )));
            }
            Err(err) => println!("  verify         skipped ({err})"),
        }
    }
    Ok(())
}
