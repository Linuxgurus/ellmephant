//! `ellmephant model …` — the weights cache: status / fetch / rm (SPEC §6.5).

use super::ModelCommand;
use super::format::human_bytes;
use crate::error::{Error, Result};
use crate::model;

/// `ellmephant model …` — status / fetch / rm over the weights cache (SPEC §6.5).
pub fn cmd(spec: &model::Spec, command: ModelCommand) -> Result<()> {
    match command {
        ModelCommand::Status {
            repo,
            revision,
            verify,
        } => {
            let spec = spec.with_overrides(repo, revision);
            let status = model::status(&spec)?;
            print_model_status(&status);
            if verify {
                verify_weights(&spec, false)?;
            }
            Ok(())
        }
        ModelCommand::Fetch {
            repo,
            revision,
            force,
        } => {
            let mut spec = spec.with_overrides(repo, revision);
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
            let spec = spec.with_overrides(repo, revision);
            let removed = model::remove(&spec, all)?;
            println!("removed {removed} revision(s)");
            Ok(())
        }
    }
}

/// Report a `model::verify` run (SPEC §6.3/§6.5): print the `ok` or `MISMATCH` lines,
/// then fail on any mismatch. `lenient` (doctor) turns a hashing error into a
/// `skipped` line; the strict caller (`model status`) propagates it.
pub(crate) fn verify_weights(spec: &model::Spec, lenient: bool) -> Result<()> {
    let bad = match model::verify(spec) {
        Ok(bad) => bad,
        Err(err) if lenient => {
            println!("  verify         skipped ({err})");
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    if bad.is_empty() {
        println!("  verify         ok (all sha256 match)");
        return Ok(());
    }
    for line in &bad {
        println!("  verify         MISMATCH {line}");
    }
    Err(Error::Model(format!(
        "{} file(s) failed verification",
        bad.len()
    )))
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
                println!("    {:<24} {:>10}  ok", file.name, human_bytes(size));
            }
            None => println!("    {:<24} {:>10}  missing", file.name, "-"),
        }
    }
    println!("  total        {}", human_bytes(total));
}
