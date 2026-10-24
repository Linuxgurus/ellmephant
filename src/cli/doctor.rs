//! `ellmephant doctor` — paths, schema version and the health of every *derived* index
//! (links, tags, vectors), plus `--verify` for a full re-hash of the weights (PLAN §6 P9).

use rusqlite::Connection;

use super::format::human_bytes;
use super::model::verify_weights;
use crate::config::{Config, Paths};
use crate::error::Result;
use crate::model;
use crate::store::{links, maintain, project, tags, version};
use crate::{db, index};

/// Report paths, schema version and derived-index health; `verify` re-hashes the weights.
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

/// `ellmephant reparse` — rebuild the `[[link]]` graph from every body, with no model and no queue.
///
/// The graph is derived at *write* time, so a textual rewrite (rename, merge), a parser change, or a
/// binary that predates the current parser leaves edges the bodies no longer imply. `memory_reembed`
/// repairs that too, at the cost of a model load and a full re-queue; when only the graph moved, this
/// is the whole job. Output is deliberately two numbers, because that is what a caller checks against
/// `dangling` afterwards.
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

/// Paths, schema version and the configured model (PLAN §6 P9).
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
    // ★ IS THE TREE A TREE? (pre-roll rehearsal, 2026-10-23.) Seeded pens are NAMES; provisioning
    // attaches them. Until it does, every pen is a root and the ancestry rules reach nothing — a store
    // that looks healthy while `estate` has no descendants is the quietest way to ship a no-op. One
    // root (`estate`) is the invariant; more means nobody has attached the tree yet.
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
    // ★ TWO FACTS, ONE VALUE (Stet, review of #122). `owner` NULL means "nobody is answerable" —
    // but WHICH nobody? A disclaimed row keeps its author: the commons, by someone's decision.
    // A row with no author either is almost always a write bug (the HTTP door did not stamp
    // `asserted_by` before 2026-10-23), and those want an owner to CLAIM them rather than being
    // silently handed to everybody. Printed here as well as in `memory_stats` because doctor is what
    // an operator runs at the store, and stats is what an agent reads.
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
    // The vec0 table only exists once something has been written; `stored_dim` is the tell.
    let dim = index::vec::stored_dim(conn)?;
    let vectors = match dim {
        Some(_) => index::vec::count(conn)?,
        None => 0,
    };
    // Per type, because one total can no longer answer "is everything indexed" — the index holds more
    // than one kind of document now, and a project could be entirely missing while the total looks
    // healthy. Tombstones are counted separately: they are deleted, not owed.
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

    // The embed projection is fingerprinted in `meta`; a mismatch means the stored vectors were
    // built from a different document than the code now emits — a `reembed` job, not corruption.
    // A verdict about every vector may not be given while some are still owed: after a projection
    // change the index is purged and the queue does the rebuild, so "the fingerprint matches" no
    // longer implies "the vectors are there".
    let memories: i64 = conn.query_row("SELECT count(*) FROM memory", [], |r| r.get(0))?;
    let verdict = crate::store::encode::projection(conn)?;
    println!("  projection     {}", verdict.verdict(vectors, memories));
    // The last rebuild's books (soma/ellmephant#76): the estimate before it started, the REFUSAL if
    // it never did, and the actual once the queue drained. This is the line that makes "the rebuild
    // cost X" answerable after the fact.
    if let Some(record) = db::meta::get(conn, maintain::REBUILD)? {
        println!("  rebuild        {record}");
    }

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
    // The version side table (migration 0011). It is SOURCE data, not a derived index, so it is
    // reported like the store's own size rather than checked like a graph — with one number that
    // matters: how many of those versions belong to a key whose memory is GONE. For an orphan the
    // newest version IS the content, so a retention pass must never thin those away.
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
