//! The CLI: one clap tree, three faces (REPL, one-shot, MCP) over one core.

pub mod bench;
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
use crate::search::Query;
use crate::store::Memory;
use crate::store::stats;

use self::session::Session;

/// Top-level arguments. `--home` (or `ELLMEPHANT_HOME`) relocates the whole store,
/// which is what keeps tests hermetic.
#[derive(Debug, Parser)]
#[command(
    name = "ellmephant",
    version = crate::VERSION,
    about = "Single-binary, SQLite-backed long-term memory for AI agents",
    long_about = None,
)]
pub struct Cli {
    /// Store root (defaults to `~/.ellmephant`).
    #[arg(long, global = true, value_name = "DIR", env = "ELLMEPHANT_HOME")]
    pub home: Option<PathBuf>,

    /// Never touch the network; a cold weights cache is an error (SPEC §6.4).
    #[arg(long, global = true, env = "ELLMEPHANT_OFFLINE")]
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
        /// Slug; auto-generated from the title when omitted.
        #[arg(long)]
        key: Option<String>,
        /// One-liner.
        #[arg(long)]
        title: String,
        /// Fact; `-` reads stdin.
        #[arg(long)]
        body: String,
        /// Provenance (command/path/repo/quote).
        #[arg(long)]
        source: Option<String>,
        /// Repeatable tag (an aspect of the subject).
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Semantic phrase search over the embedded document.
    Search {
        /// The phrase.
        query: Option<String>,
        /// Result count.
        #[arg(long)]
        k: Option<usize>,
        /// Cosine-distance ceiling.
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
    /// Bump a memory's `was_useful` counter (it earned its keep).
    Useful { key: String },
    /// List the memory index (key, tags, title) without bodies.
    List {
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// List tags with counts.
    Tags {
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Digest of the store: size, spread, tags, links and activity.
    #[command(visible_alias = "summary", alias = "stat")]
    Stats {
        /// Emit JSON.
        #[arg(long)]
        json: bool,
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
    /// Serve MCP over Streamable HTTP (default) or stdio.
    Mcp {
        /// Serve over stdio instead of HTTP.
        #[arg(long)]
        stdio: bool,
        /// HTTP bind address (use `0.0.0.0` in Docker).
        #[arg(
            long,
            value_name = "ADDR",
            env = "ELLMEPHANT_MCP_HOST",
            default_value = "127.0.0.1"
        )]
        host: String,
        /// HTTP port (default: config `port`, else 8888).
        #[arg(long, value_name = "PORT", env = "ELLMEPHANT_MCP_PORT")]
        port: Option<u16>,
    },
    /// Weights cache: status | fetch | rm (SPEC §6.5).
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Measure load/embed/search latency; `--golden` scores retrieval quality.
    Bench {
        /// Run the golden eval set too.
        #[arg(long)]
        golden: bool,
        /// Golden JSONL (default: `<home>/golden/queries.jsonl`).
        #[arg(long, value_name = "FILE")]
        golden_file: Option<PathBuf>,
        /// Sweep the result count and the embed projection.
        #[arg(long)]
        sweep: bool,
        /// Result count for the timing search and the single-shot golden run.
        #[arg(long, default_value_t = 10)]
        k: usize,
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

/// `ellmephant model …` — the weights cache (SPEC §6.5).
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
    let mut cfg = Config::load(&paths)?;
    if let Some(device) = cli.device.clone() {
        cfg.device = device;
    }
    if let Some(precision) = cli.precision.clone() {
        cfg.precision = precision;
    }
    // Flags override the file, so re-check here too.
    cfg.validate()?;

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
        Command::Mcp { stdio, host, port } => {
            let transport = if stdio {
                crate::mcp::Transport::Stdio
            } else {
                crate::mcp::Transport::Http {
                    host,
                    port: port.unwrap_or(cfg.port),
                }
            };
            crate::mcp::serve(paths, cfg, spec, transport)
        }
        Command::Bench {
            golden,
            golden_file,
            sweep,
            k,
        } => bench::run(
            &paths,
            &cfg,
            &spec,
            &bench::Options {
                golden,
                golden_file,
                sweep,
                k,
            },
        ),
        command => {
            let mut session = Session::open(paths, cfg, spec)?;
            execute(&mut session, command)
        }
    }
}

/// Run one already-parsed store command against a live [`Session`] — the shared core of
/// the one-shot CLI and the REPL, so the two can never drift apart (PLAN §6 P6).
/// The dispatcher only builds the request; each `cmd_*` does the work and the printing.
pub fn execute(session: &mut Session, command: Command) -> Result<()> {
    match command {
        Command::Add {
            key,
            title,
            body,
            source,
            tags,
        } => {
            let mem = Memory {
                key: key.unwrap_or_default(),
                title,
                body: read_body(&body)?,
                source,
                tags,
                ..Default::default()
            };
            cmd_add(session, &mem)
        }
        Command::Search {
            query,
            k,
            max_distance,
            json,
            expand,
        } => {
            let q = Query {
                text: query,
                k: k.unwrap_or_else(|| session.default_k()),
                max_distance: max_distance.or(session.default_max_distance()),
                expand,
            };
            cmd_search(session, q, json)
        }
        Command::Show { key, json } => cmd_show(session, &key, json),
        Command::Rm { key } => cmd_rm(session, &key),
        Command::Useful { key } => cmd_useful(session, &key),
        Command::List { json } => cmd_list(session, json),
        Command::Tags { json } => cmd_tags(session, json),
        Command::Stats { json } => cmd_stats(session, json),
        Command::Rename { old, new } => cmd_rename(session, &old, &new),
        Command::Reembed => cmd_reembed(session),
        Command::Links { json } => cmd_links(session, json),
        // Not store commands: the dispatcher above owns these, and the REPL rejects them.
        Command::Repl
        | Command::Doctor { .. }
        | Command::Model { .. }
        | Command::Embed { .. }
        | Command::Bench { .. }
        | Command::Mcp { .. } => Err(Error::Config("command is not available here".into())),
    }
}

fn cmd_add(session: &mut Session, mem: &Memory) -> Result<()> {
    let key = session.add(mem)?;
    println!("added {key}");
    Ok(())
}

fn cmd_search(session: &mut Session, q: Query, json: bool) -> Result<()> {
    let bundle = session.search(&q)?;
    emit(format::render_bundle(&bundle, json)?);
    Ok(())
}

fn cmd_show(session: &Session, key: &str, json: bool) -> Result<()> {
    // The read path counts a visit (telemetry); the write path's existence check does not.
    let mem = session.fetch(key)?;
    emit(format::render_memory(&mem, json)?);
    Ok(())
}

fn cmd_useful(session: &Session, key: &str) -> Result<()> {
    let total = session.record_useful(key)?;
    println!("useful {key} ({total})");
    Ok(())
}

fn cmd_rm(session: &Session, key: &str) -> Result<()> {
    if session.rm(key)? {
        println!("removed {key}");
        Ok(())
    } else {
        Err(Error::NotFound(key.to_string()))
    }
}

fn cmd_list(session: &Session, json: bool) -> Result<()> {
    let rows = session.summaries()?;
    emit(format::render_summaries(&rows, json)?);
    Ok(())
}

fn cmd_tags(session: &Session, json: bool) -> Result<()> {
    let tags = session.tags()?;
    emit(format::render_tags(&tags, json)?);
    Ok(())
}

fn cmd_stats(session: &Session, json: bool) -> Result<()> {
    // The store layer is DB-only; the on-disk size is a filesystem fact the CLI supplies.
    let mut stats = session.stats()?;
    stats.db_bytes = stats::disk_bytes(&session.paths().db());
    emit(format::render_stats(&stats, json)?);
    Ok(())
}

fn cmd_rename(session: &Session, old: &str, new: &str) -> Result<()> {
    let outcome = session.rename(old, new)?;
    println!(
        "renamed {} -> {} ({} body/bodies rewritten, {} edge(s) repointed)",
        old, outcome.key, outcome.bodies, outcome.edges
    );
    Ok(())
}

fn cmd_reembed(session: &mut Session) -> Result<()> {
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

fn cmd_links(session: &Session, json: bool) -> Result<()> {
    let dangling = session.dangling()?;
    emit(format::render_dangling(&dangling, json)?);
    Ok(())
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
