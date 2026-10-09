//! Black-box CLI tests. `LLMPHANT_HOME` points every run at a throwaway store, so
//! these never touch the real `~/.llmphant`.

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_llmphant"))
}

fn run(home: &Path, args: &[&str]) -> Output {
    bin()
        .args(args)
        .env("LLMPHANT_HOME", home)
        .output()
        .expect("spawn llmphant")
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
    assert!(text.contains("llmphant"), "got: {text}");
    assert!(text.contains(env!("CARGO_PKG_VERSION")), "got: {text}");
}

#[test]
fn help_lists_the_subcommands() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["--help"]);
    assert!(out.status.success());
    let text = stdout(&out);
    for cmd in ["doctor", "search", "add", "mcp", "export"] {
        assert!(text.contains(cmd), "help is missing {cmd}: {text}");
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
    for needle in ["schema", "v1", "memory.db", "model"] {
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
fn unimplemented_subcommands_fail_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["search", "anything"]);
    assert!(
        !out.status.success(),
        "an unimplemented path must not exit 0"
    );
    let text = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(text.contains("not implemented"), "got: {text}");
}
