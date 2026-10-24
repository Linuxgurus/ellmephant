//! The binary's command surface: the MCP server, plus two operational commands.
//!
//! The interactive REPL and the one-shot store commands (add/search/show/stats/…) are gone:
//! MCP is the read/write path, and anything that is not MCP is better served by the API.

pub mod doctor;
pub mod embed;
pub mod format;
pub mod model;
mod pen;
pub mod session;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::{Config, Paths};
use crate::error::Result;

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

/// What the binary can be asked to do. With no subcommand it serves MCP.
#[derive(Debug, Subcommand)]
pub enum Command {
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
    /// Print paths, schema version and derived-index health.
    Doctor {
        /// Re-hash every cached weight file (slow; SPEC §6.3).
        #[arg(long)]
        verify: bool,
    },
    /// Rebuild the `[[link]]` graph from every body. No model, no queue — milliseconds.
    Reparse,
    /// The PROVISIONING door: attach | mint | tree. Standing, on purpose.
    Pen {
        #[command(subcommand)]
        command: PenCommand,
    },
    /// Weights cache: status | fetch | rm (SPEC §6.5).
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
}

/// `ellmephant pen …` — the provisioning door (2026-10-24): the runtime door lets a live pen create a pen
/// under itself; this one lets whoever can open the database say what a pen IS. It is not a new authority,
/// only a way to reach `set_parent` — because until now the attach had no mechanism at all, and the tree
/// had to be smuggled in as boot configuration.
#[derive(Debug, Subcommand)]
pub enum PenCommand {
    /// Move pens: `child=parent` (overwrites what the table says — this is the deliberate act).
    Attach {
        /// One or more `child=parent` edges.
        #[arg(value_name = "CHILD=PARENT", required = true)]
        edges: Vec<String>,
    },
    /// Create a pen by provisioning: `--parent` is required, and `anonymous` is refused.
    Mint {
        /// The new pen's name.
        #[arg(value_name = "NAME")]
        name: String,
        /// The pen it answers to.
        #[arg(long, value_name = "PARENT")]
        parent: String,
        /// `self` (an identity) or `finger` (a session key). Defaults to `self`.
        #[arg(long, value_name = "KIND", default_value = "self")]
        kind: String,
    },
    /// Print the shape and the root count.
    Tree,
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

/// Resolve paths, then dispatch. No subcommand means "serve MCP" — the container's mode.
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

    let command = cli.command.unwrap_or_else(|| Command::Mcp {
        stdio: false,
        host: std::env::var("ELLMEPHANT_MCP_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
        port: None,
    });

    match command {
        Command::Doctor { verify } => doctor::run(&paths, &cfg, &spec, verify),
        Command::Reparse => doctor::reparse(&paths, &cfg),
        Command::Pen { command } => match command {
            PenCommand::Attach { edges } => pen::attach(&paths, &cfg, &edges),
            PenCommand::Mint { name, parent, kind } => {
                pen::mint(&paths, &cfg, &name, &parent, &kind)
            }
            PenCommand::Tree => pen::tree(&paths, &cfg),
        },
        Command::Model { command } => model::cmd(&spec, command),
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
    }
}
