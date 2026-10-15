//! `ellmephant` binary: parse arguments and hand off to the CLI.

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use ellmephant::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();

    // `RUST_LOG=ellmephant=debug`; silent by default so stdout stays machine-readable.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    // A genesis line, so a pod's log starts with WHAT was started and under which knobs. The
    // EFFECTIVE device/precision are logged later by the session that resolves them (the flags here
    // are only the raw request, and `None` means "take the default").
    tracing::info!(
        version = ellmephant::VERSION,
        offline = cli.offline,
        home = ?cli.home,
        model_dir = ?cli.model_dir,
        "ellmephant starting"
    );

    match ellmephant::cli::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
