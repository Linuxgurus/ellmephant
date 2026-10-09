//! The CLI: one clap tree, three faces (REPL, one-shot, MCP) over one core.

pub mod format;
pub mod repl;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::{Config, Paths};
use crate::error::{Error, Result};
use crate::search::Mode;
use crate::{db, index};

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
    /// Measure load/embed/search latency.
    Bench {
        /// Run the golden eval set too.
        #[arg(long)]
        golden: bool,
    },
}

/// Resolve paths, then dispatch.
pub fn run(cli: Cli) -> Result<()> {
    let paths = match cli.home {
        Some(dir) => Paths::from_root(dir),
        None => Paths::discover()?,
    };
    match cli.command.unwrap_or(Command::Repl) {
        Command::Repl => repl::run(&paths),
        Command::Doctor => doctor(&paths),
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

/// `llmphant doctor` — paths, schema version, table count, embedded-index status.
pub fn doctor(paths: &Paths) -> Result<()> {
    let cfg = Config::load(paths)?;
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
    let vec_status = match index::vec::stored_dim(&conn)? {
        Some(dim) => format!("vec0 dim={dim}, {} rows", index::vec::count(&conn)?),
        None => "registered (no vectors yet)".to_string(),
    };
    println!("  vec0           {vec_status}");
    Ok(())
}
