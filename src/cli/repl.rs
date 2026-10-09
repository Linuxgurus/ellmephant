//! Interactive REPL (P6): rustyline, history at `~/.llmphant/history`, tab
//! completion for commands and keys.
//!
//! Commands mirror the one-shot subcommands: `add search show rm tags link reembed
//! export import help quit`.

use crate::config::Paths;
use crate::error::{Error, Result};

/// Run the REPL until `quit`/EOF.
pub fn run(paths: &Paths) -> Result<()> {
    paths.ensure()?;
    eprintln!(
        "llmphant {} — REPL is not wired yet (PLAN.md P6).\nStore root: {}",
        crate::VERSION,
        paths.root().display()
    );
    Err(Error::NotImplemented("interactive REPL (PLAN.md P6)"))
}
