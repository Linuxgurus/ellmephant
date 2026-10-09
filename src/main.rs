//! `llmphant` binary: parse arguments and hand off to the CLI.

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use llmphant::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();

    // `RUST_LOG=llmphant=debug`; silent by default so stdout stays machine-readable.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    match llmphant::cli::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
