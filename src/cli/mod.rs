//! The CLI: one clap tree, three faces (REPL, one-shot, MCP) over one core.

pub mod doctor;
pub mod embed;
pub mod format;
pub mod model;
pub mod repl;
pub mod session;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::{Config, Paths};
use crate::error::{Error, Result};
use crate::search::{Mode, Query};
use crate::store::{Memory, MemoryType};

use self::session::Session;

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

    /// Embedding device: `cpu` or `metal` (SPEC §15 #9).
    #[arg(long, global = true, value_name = "DEV")]
    pub device: Option<String>,

    /// Weight precision: `bf16` (default) or `f16` (SPEC §6).
    #[arg(long, global = true, value_name = "DTYPE")]
    pub precision: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// One-shot subcommands; the REPL mirrors these.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Interactive REPL (default when no subcommand is given).
    Repl,
    /// Print paths, schema version and derived-index health.
    Doctor {
        /// Re-hash every cached weight file (slow; SPEC §6.3).
        #[arg(long)]
        verify: bool,
    },
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
        /// Cosine-distance ceiling (KNN leg only).
        #[arg(long)]
        max_distance: Option<f32>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
        /// Link-neighbourhood hops to include.
        #[arg(long, default_value_t = 0)]
        expand: u8,
    },
    /// Print one memory by key.
    Show {
        key: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Delete one memory by key.
    Rm { key: String },
    /// List tags with counts, or `tags prune`.
    Tags {
        /// Emit JSON (list mode).
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        command: Option<TagCommand>,
    },
    /// Recompute every vector and rebuild the vector index (handles a model/dim change).
    Reembed,
    /// Rename a key: rewrite `[[old]]` in bodies and repoint link edges.
    Rename {
        /// Existing key.
        old: String,
        /// New key.
        new: String,
    },
    /// Audit the link graph for dangling `[[key]]` targets.
    Links {
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
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
    /// Embed a string and print dim + latency (debug; PLAN P3).
    Embed {
        /// Text to embed.
        text: String,
        /// Embed as a query (applies the instruction prefix).
        #[arg(long)]
        query: bool,
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

/// `llmphant tags …` — tag governance (PLAN §2 #6).
#[derive(Debug, Subcommand)]
pub enum TagCommand {
    /// Drop tags no memory references.
    Prune {
        /// Report how many would be dropped without writing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Resolve paths, then dispatch.
pub fn run(cli: Cli) -> Result<()> {
    let paths = match cli.home {
        Some(dir) => Paths::from_root(dir),
        None => Paths::discover()?,
    };
    let mut cfg = Config::load(&paths)?;
    if let Some(device) = cli.device.clone() {
        cfg.device = device;
    }
    if let Some(precision) = cli.precision.clone() {
        cfg.precision = precision;
    }

    // Flags override config (SPEC §12); `--model-dir` is inherently offline.
    let mut spec = crate::model::Spec::from_config(&cfg, &paths);
    if cli.offline {
        spec.offline = true;
    }
    if let Some(dir) = cli.model_dir {
        spec.model_dir = Some(dir);
        spec.offline = true;
    }

    match cli.command.unwrap_or(Command::Repl) {
        Command::Repl => {
            let session = Session::open(paths, cfg, spec)?;
            repl::run(session)
        }
        Command::Doctor { verify } => doctor::run(&paths, &cfg, &spec, verify),
        Command::Model { command } => model::cmd(&spec, command),
        Command::Embed { text, query } => embed::run(&spec, &cfg, &text, query),
        // MCP holds the store open and serves until the client disconnects (P7).
        Command::Mcp => crate::mcp::serve(paths, cfg, spec),
        Command::Bench { .. } => Err(Error::NotImplemented("bench (PLAN.md P10)")),
        command => {
            let mut session = Session::open(paths, cfg, spec)?;
            execute(&mut session, command)
        }
    }
}

/// Run one already-parsed store command against a live [`Session`] — the shared core of
/// the one-shot CLI and the REPL, so the two can never drift apart (PLAN §6 P6).
pub fn execute(session: &mut Session, command: Command) -> Result<()> {
    match command {
        Command::Add {
            key,
            scope,
            title,
            body,
            r#type,
            source,
            tags,
        } => {
            let mem = Memory {
                key: key.unwrap_or_default(),
                scope,
                type_: MemoryType::parse(&r#type),
                title,
                body: read_body(&body)?,
                source,
                tags,
                ..Default::default()
            };
            let key = session.add(&mem)?;
            println!("added {key}");
            Ok(())
        }
        Command::Search {
            query,
            scope,
            k,
            mode,
            max_distance,
            json,
            expand,
        } => {
            let q = Query {
                text: query,
                scope,
                k: k.unwrap_or_else(|| session.default_k()),
                mode: Mode::parse(&mode)?,
                max_distance,
                expand,
            };
            let bundle = session.search(&q)?;
            emit(format::render_bundle(&bundle, json)?);
            Ok(())
        }
        Command::Show { key, json } => {
            let mem = session.show(&key)?;
            emit(format::render_memory(&mem, json)?);
            Ok(())
        }
        Command::Rm { key } => {
            if session.rm(&key)? {
                println!("removed {key}");
                Ok(())
            } else {
                Err(Error::NotFound(key))
            }
        }
        Command::Tags { json, command } => match command {
            Some(TagCommand::Prune { dry_run }) => {
                if dry_run {
                    let orphans = session.orphans()?.len();
                    println!("{orphans} orphan tag(s) would be pruned");
                } else {
                    let pruned = session.prune_tags()?;
                    println!("pruned {pruned} orphan tag(s)");
                }
                Ok(())
            }
            None => {
                let tags = session.tags()?;
                emit(format::render_tags(&tags, json)?);
                Ok(())
            }
        },
        Command::Rename { old, new } => {
            let outcome = session.rename(&old, &new)?;
            println!(
                "renamed {} -> {} ({} body/bodies rewritten, {} edge(s) repointed)",
                old, outcome.key, outcome.bodies, outcome.edges
            );
            Ok(())
        }
        Command::Reembed => {
            let report = session.reembed()?;
            println!(
                "re-embedded {} row(s) at dim {}{} ({})",
                report.rows,
                report.dim,
                if report.rebuilt {
                    "; vector index recreated"
                } else {
                    ""
                },
                report.model
            );
            Ok(())
        }
        Command::Links { json } => {
            let dangling = session.dangling()?;
            emit(format::render_dangling(&dangling, json)?);
            Ok(())
        }
        Command::Export { path } => {
            let count = session.export(&path)?;
            println!("exported {count} memories to {}", path.display());
            Ok(())
        }
        Command::Import { path, dry_run } => {
            let count = session.import(&path, dry_run)?;
            if dry_run {
                println!("{count} memory change(s) pending from {}", path.display());
            } else {
                println!("imported {count} memory change(s) from {}", path.display());
            }
            Ok(())
        }
        // Not store commands: the dispatcher above owns these, and the REPL rejects them.
        Command::Repl
        | Command::Doctor { .. }
        | Command::Model { .. }
        | Command::Embed { .. }
        | Command::Mcp => Err(Error::Config("command is not available here".into())),
        Command::Bench { .. } => Err(Error::NotImplemented("bench (PLAN.md P10)")),
    }
}

/// Print rendered output, guaranteeing a trailing newline so the next prompt starts clean.
fn emit(text: String) {
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
}

/// `--body -` reads the body from stdin (long, piped memories); anything else is literal.
fn read_body(arg: &str) -> Result<String> {
    if arg != "-" {
        return Ok(arg.to_string());
    }
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}
