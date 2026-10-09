//! The CLI: one clap tree, three faces (REPL, one-shot, MCP) over one core.

pub mod format;
pub mod repl;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::{Config, Paths};
use crate::error::{Error, Result};
use crate::search::Mode;
use crate::{db, index, model};

/// Top-level arguments. `--home` (or `LLMPHANT_HOME`) relocates the whole store,
/// which is what keeps tests hermetic.
#[derive(Debug, Parser)]
#[command(
    name = "llmphant",
    version = crate::VERSION,
    about = "Single-binary, SQLite-backed long-term memory for AI agents",
    long_about = None,
)]
pub struct Cli {
    /// Store root (defaults to `~/.llmphant`).
    #[arg(long, global = true, value_name = "DIR", env = "LLMPHANT_HOME")]
    pub home: Option<PathBuf>,

    /// Never touch the network; a cold weights cache is an error (SPEC §6.4).
    #[arg(long, global = true, env = "LLMPHANT_OFFLINE")]
    pub offline: bool,

    /// Use an existing flat weights directory instead of the cache (implies `--offline`).
    #[arg(long, global = true, value_name = "DIR")]
    pub model_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// One-shot subcommands; the REPL mirrors these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Interactive REPL (default when no subcommand is given).
    Repl,
    /// Print paths, schema version and index status.
    Doctor,
    /// Store a memory (upsert by key).
    Add {
        /// Slug; auto-generated from scope + title when omitted.
        #[arg(long)]
        key: Option<String>,
        /// Bucket, e.g. `infra`.
        #[arg(long)]
        scope: String,
        /// Salient one-liner.
        #[arg(long)]
        title: String,
        /// Full body; `-` reads stdin.
        #[arg(long)]
        body: String,
        /// `signpost|dictate|fact|profile`.
        #[arg(long, default_value = "fact")]
        r#type: String,
        /// Provenance (command/path/repo/quote).
        #[arg(long)]
        source: Option<String>,
        /// Repeatable tag.
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Hybrid suffix search.
    Search {
        /// Query text.
        query: String,
        /// Restrict to one bucket.
        #[arg(long)]
        scope: Option<String>,
        /// Result count.
        #[arg(long)]
        k: Option<usize>,
        /// `hybrid|vec|fts`.
        #[arg(long, default_value = "hybrid")]
        mode: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
        /// Link-neighbourhood hops to include.
        #[arg(long, default_value_t = 0)]
        expand: u8,
    },
    /// Print one memory by key.
    Show { key: String },
    /// Delete one memory by key.
    Rm { key: String },
    /// List tags with counts.
    Tags,
    /// Export the store to JSONL.
    Export { path: PathBuf },
    /// Import JSONL into the store.
    Import {
        path: PathBuf,
        /// Report what would change without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Serve MCP over stdio.
    Mcp,
    /// Weights cache: status | fetch | rm (SPEC §6.5).
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Measure load/embed/search latency.
    Bench {
        /// Run the golden eval set too.
        #[arg(long)]
        golden: bool,
    },
}

/// `llmphant model …` — the weights cache (SPEC §6.5).
#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Show the cache without touching the network.
    Status {
        /// Inspect this repo instead of the configured one.
        #[arg(long)]
        repo: Option<String>,
        /// Inspect this revision instead of the configured one.
        #[arg(long)]
        revision: Option<String>,
        /// Recompute every file's sha256 (slow; SPEC §6.3).
        #[arg(long)]
        verify: bool,
    },
    /// Download or repair the weights cache.
    Fetch {
        /// Override the configured repo id.
        #[arg(long)]
        repo: Option<String>,
        /// Override the configured revision (ref or sha).
        #[arg(long)]
        revision: Option<String>,
        /// Re-download even if the cache looks complete.
        #[arg(long)]
        force: bool,
    },
    /// Remove cached revisions.
    Rm {
        /// Target this repo instead of the configured one.
        #[arg(long)]
        repo: Option<String>,
        /// Revision to drop (default: the configured one).
        #[arg(long)]
        revision: Option<String>,
        /// Drop every cached revision of the repo.
        #[arg(long)]
        all: bool,
    },
}

/// Resolve paths, then dispatch.
pub fn run(cli: Cli) -> Result<()> {
    let paths = match cli.home {
        Some(dir) => Paths::from_root(dir),
        None => Paths::discover()?,
    };
    let cfg = Config::load(&paths)?;

    // Flags override config (SPEC §12); `--model-dir` is inherently offline.
    let mut spec = model::Spec::from_config(&cfg, &paths);
    if cli.offline {
        spec.offline = true;
    }
    if let Some(dir) = cli.model_dir {
        spec.model_dir = Some(dir);
        spec.offline = true;
    }

    match cli.command.unwrap_or(Command::Repl) {
        Command::Repl => repl::run(&paths),
        Command::Doctor => doctor(&paths, &cfg, &spec),
        Command::Model { command } => model_cmd(&spec, command),
        Command::Add { .. } => Err(Error::NotImplemented("add (PLAN.md P4)")),
        Command::Search { mode, .. } => {
            Mode::parse(&mode)?;
            Err(Error::NotImplemented("search (PLAN.md P5)"))
        }
        Command::Show { .. } => Err(Error::NotImplemented("show (PLAN.md P4)")),
        Command::Rm { .. } => Err(Error::NotImplemented("rm (PLAN.md P4)")),
        Command::Tags => Err(Error::NotImplemented("tags (PLAN.md P9)")),
        Command::Export { .. } => Err(Error::NotImplemented("export (PLAN.md P8)")),
        Command::Import { .. } => Err(Error::NotImplemented("import (PLAN.md P8)")),
        Command::Mcp => Err(Error::NotImplemented("mcp server (PLAN.md P7)")),
        Command::Bench { .. } => Err(Error::NotImplemented("bench (PLAN.md P10)")),
    }
}

/// `llmphant model …` — status / fetch / rm over the weights cache (SPEC §6.5).
fn model_cmd(spec: &model::Spec, command: ModelCommand) -> Result<()> {
    match command {
        ModelCommand::Status {
            repo,
            revision,
            verify,
        } => {
            let mut spec = spec.clone();
            if let Some(repo) = repo {
                spec.repo = repo;
            }
            if let Some(revision) = revision {
                spec.revision = revision;
            }
            let status = model::status(&spec)?;
            print_model_status(&status);
            if verify {
                let bad = model::verify(&spec)?;
                if bad.is_empty() {
                    println!("  verify         ok (all sha256 match)");
                } else {
                    for line in &bad {
                        println!("  verify         MISMATCH {line}");
                    }
                    return Err(Error::Model(format!(
                        "{} file(s) failed verification",
                        bad.len()
                    )));
                }
            }
            Ok(())
        }
        ModelCommand::Fetch {
            repo,
            revision,
            force,
        } => {
            let mut spec = spec.clone();
            if let Some(repo) = repo {
                spec.repo = repo;
            }
            if let Some(revision) = revision {
                spec.revision = revision;
            }
            spec.force = force;
            let fetcher = model::fetch::UreqFetcher::from_env();
            let dir = model::ensure(&spec, &fetcher)?;
            println!("weights ready: {}", dir.display());
            Ok(())
        }
        ModelCommand::Rm {
            repo,
            revision,
            all,
        } => {
            let mut spec = spec.clone();
            if let Some(repo) = repo {
                spec.repo = repo;
            }
            if let Some(revision) = revision {
                spec.revision = revision;
            }
            let removed = model::remove(&spec, all)?;
            println!("removed {removed} revision(s)");
            Ok(())
        }
    }
}

/// Render a [`model::Status`] as a human-readable block.
fn print_model_status(status: &model::Status) {
    let resolved = match &status.resolved {
        Some(sha) => format!(" -> {sha}"),
        None => " (unresolved)".to_string(),
    };
    println!("model          {}", status.repo);
    println!("  revision     {}{resolved}", status.revision);
    println!("  dir          {}", status.dir.display());
    println!(
        "  manifest     {}",
        if status.manifest { "present" } else { "absent" }
    );
    let mut total = 0u64;
    for file in &status.files {
        match file.size {
            Some(size) => {
                total += size;
                println!("    {:<24} {:>10}  ok", file.name, human(size));
            }
            None => println!("    {:<24} {:>10}  missing", file.name, "-"),
        }
    }
    println!("  total        {}", human(total));
}

/// Bytes -> a short human string (`1.2 GiB`).
fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `llmphant doctor` — paths, schema version, table count, embedded-index status.
pub fn doctor(paths: &Paths, cfg: &Config, spec: &model::Spec) -> Result<()> {
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
        human(bytes),
        if weights.offline { " (offline)" } else { "" }
    );

    let vec_status = match index::vec::stored_dim(&conn)? {
        Some(dim) => format!("vec0 dim={dim}, {} rows", index::vec::count(&conn)?),
        None => "registered (no vectors yet)".to_string(),
    };
    println!("  vec0           {vec_status}");
    Ok(())
}
