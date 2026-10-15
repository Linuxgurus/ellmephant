//! Black-box CLI tests. `ELLMEPHANT_HOME` points every run at a throwaway store, so
//! these never touch the real `~/.ellmephant`.

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ellmephant"))
}

fn run(home: &Path, args: &[&str]) -> Output {
    bin()
        .args(args)
        .env("ELLMEPHANT_HOME", home)
        .output()
        .expect("spawn ellmephant")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn version_reports_the_package_version() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["--version"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("ellmephant"), "got: {text}");
    assert!(text.contains(env!("CARGO_PKG_VERSION")), "got: {text}");
}

#[test]
fn help_lists_the_surviving_subcommands() {
    // The REPL and the one-shot store commands were removed; what remains is the MCP
    // server plus the two operational commands.
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["--help"]);
    assert!(out.status.success());
    let text = stdout(&out);
    for cmd in ["mcp", "doctor", "model"] {
        assert!(text.contains(cmd), "help is missing {cmd}: {text}");
    }
    for gone in ["repl", "search", "add", "rename", "links", "bench"] {
        assert!(!text.contains(gone), "help still advertises {gone}: {text}");
    }
}

#[test]
fn doctor_bootstraps_a_fresh_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let out = run(home, &["doctor"]);
    assert!(
        out.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let text = stdout(&out);
    // Track the binary's own schema version rather than hard-coding it.
    let schema = format!("v{}", ellmephant::db::migrate::latest());
    for needle in ["schema", schema.as_str(), "memory.db", "model"] {
        assert!(
            text.contains(needle),
            "doctor output missing {needle:?}: {text}"
        );
    }
    assert!(
        home.join("memory.db").exists(),
        "doctor did not create the store"
    );
    assert!(
        home.join("models").is_dir(),
        "doctor did not create models/"
    );
}

#[test]
fn an_invalid_device_flag_is_rejected_before_any_model_load() {
    // Flags override the config file, so they are validated too (REFACTOR follow-up):
    // a bad `--device` must fail fast with one clear message, not at encoder load.
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["--device", "tpu", "doctor"]);
    assert!(!out.status.success(), "a bad device must not exit 0");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("cpu|metal"), "got: {text}");
}

#[test]
fn model_status_reports_an_empty_cache_without_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["model", "status"]);
    assert!(
        out.status.success(),
        "model status must work offline: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    for needle in ["revision", "manifest", "total"] {
        assert!(text.contains(needle), "missing {needle:?}: {text}");
    }
    assert!(
        text.contains("absent"),
        "an untouched cache has no manifest: {text}"
    );
}

#[test]
fn model_rm_is_a_no_op_on_an_empty_cache() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["model", "rm"]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("removed 0 revision(s)"),
        "got: {}",
        stdout(&out)
    );
}

#[test]
fn model_status_verify_fails_loudly_on_an_empty_cache() {
    // The strict path: `model status --verify` propagates a verification error rather than
    // printing a `skipped` line (that leniency belongs to `doctor`).
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["model", "status", "--verify"]);
    assert!(!out.status.success(), "verifying nothing must not exit 0");
    assert!(
        !out.stderr.is_empty(),
        "a failure must say something on stderr"
    );
}

#[test]
fn doctor_verify_is_lenient_about_missing_weights() {
    // The other half of the pair: `doctor --verify` degrades to a `skipped` line instead of
    // failing, because a cluster without egress still has to boot (SPEC §6.4).
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["doctor", "--verify"]);
    assert!(
        out.status.success(),
        "doctor --verify must not fail offline: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains("skipped"), "got: {}", stdout(&out));
}

#[test]
fn the_home_flag_beats_the_env_and_offline_flags_are_accepted() {
    // `--home` picks the store root directly, the branch the env var does not take; and the
    // offline/model-dir flags override config (SPEC §12) without touching the network.
    let env_home = tempfile::tempdir().unwrap();
    let flag_home = tempfile::tempdir().unwrap();
    let models = tempfile::tempdir().unwrap();

    let out = run(
        env_home.path(),
        &[
            "--home",
            flag_home.path().to_str().unwrap(),
            "--offline",
            "--model-dir",
            models.path().to_str().unwrap(),
            "doctor",
        ],
    );
    assert!(
        out.status.success(),
        "got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        flag_home.path().join("memory.db").exists(),
        "--home must win over ELLMEPHANT_HOME"
    );
    assert!(
        !env_home.path().join("memory.db").exists(),
        "the env home must have been ignored"
    );
}
