//! Black-box CLI tests. `LLMPHANT_HOME` points every run at a throwaway store, so
//! these never touch the real `~/.llmphant`.

use std::fs::File;
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
fn embed_offline_with_a_cold_cache_names_the_model() {
    // No weights in a throwaway home, and `--offline` forbids fetching: the command must
    // fail cleanly (cache layer, before any candle work) rather than hang or panic.
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["--offline", "embed", "hello"]);
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("offline"), "got: {text}");
    assert!(
        text.contains("Qwen/Qwen3-Embedding-0.6B"),
        "error should name the model: {text}"
    );
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
    // `mcp` (P7) is still a stub; it must exit non-zero rather than pretend to serve.
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["mcp"]);
    assert!(
        !out.status.success(),
        "an unimplemented path must not exit 0"
    );
    let text = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(text.contains("not implemented"), "got: {text}");
}

#[test]
fn fts_search_needs_no_model_and_reports_no_matches() {
    // `--mode fts` must not load 1.2 GB of weights: a fresh store searches lexically and
    // exits cleanly with an empty result set.
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["search", "anything", "--mode", "fts"]);
    assert!(
        out.status.success(),
        "fts search failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains("no matches"), "got: {}", stdout(&out));
}

#[test]
fn fts_search_json_is_machine_readable() {
    // `--json` is the stable face of a command (PLAN §6 P6): a fresh store still emits a
    // well-formed object, so scripts never have to special-case the empty result.
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        dir.path(),
        &["search", "anything", "--mode", "fts", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(value["hits"], serde_json::json!([]));
}

#[test]
fn tags_on_a_fresh_store_are_empty() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["tags"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains("no tags"), "got: {}", stdout(&out));

    let json = run(dir.path(), &["tags", "--json"]);
    assert!(json.status.success());
    let value: serde_json::Value = serde_json::from_str(&stdout(&json)).expect("valid JSON");
    assert_eq!(value, serde_json::json!([]));
}

#[test]
fn repl_records_history_across_sessions() {
    // The REPL feeds a scripted stdin: commands run, and `history` is written so the next
    // session can recall them (PLAN §6 P6 acceptance).
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = home.join("session.txt");

    std::fs::write(&script, "tags\nhelp\nquit\n").unwrap();
    let first = bin()
        .env("LLMPHANT_HOME", home)
        .stdin(File::open(&script).unwrap())
        .output()
        .expect("spawn repl");
    assert!(
        first.status.success(),
        "repl failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let history = std::fs::read_to_string(home.join("history")).expect("history file");
    assert!(history.contains("tags"), "got: {history}");

    // A second session loads the previous history and appends to it.
    std::fs::write(&script, "quit\n").unwrap();
    bin()
        .env("LLMPHANT_HOME", home)
        .stdin(File::open(&script).unwrap())
        .output()
        .expect("spawn repl");
    let history = std::fs::read_to_string(home.join("history")).unwrap();
    assert!(history.contains("tags"), "history not persisted: {history}");
    assert!(history.contains("quit"), "history not persisted: {history}");
}

#[test]
fn show_and_rm_name_a_missing_key() {
    // `show`/`rm` (P6) on an unknown key must exit non-zero and say *which* key, rather
    // than silently doing nothing.
    let dir = tempfile::tempdir().unwrap();
    for args in [["show", "ghost"], ["rm", "ghost"]] {
        let out = run(dir.path(), &args);
        assert!(!out.status.success(), "{args:?} should fail");
        let text = String::from_utf8_lossy(&out.stderr).to_lowercase();
        assert!(
            text.contains("ghost"),
            "{args:?} should name the key: {text}"
        );
    }
}
