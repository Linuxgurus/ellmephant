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
    for cmd in [
        "doctor", "search", "add", "mcp", "reembed", "rename", "links", "bench", "list",
    ] {
        assert!(text.contains(cmd), "help is missing {cmd}: {text}");
    }
}

#[test]
fn maintenance_commands_behave_on_a_fresh_store() {
    // The maintenance commands must work without the model: `rename`, `links` and `tags`
    // touch only the relational store, so a cold home is enough to exercise them end to end.
    let dir = tempfile::tempdir().unwrap();

    let out = run(dir.path(), &["links"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout(&out).contains("no dangling links"),
        "got: {}",
        stdout(&out)
    );

    let out = run(dir.path(), &["list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout(&out).contains("no memories"),
        "got: {}",
        stdout(&out)
    );

    let out = run(dir.path(), &["list", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out).trim(), "[]", "got: {}", stdout(&out));

    let out = run(dir.path(), &["tags"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains("no tags"), "got: {}", stdout(&out));

    let out = run(dir.path(), &["rename", "ghost", "new"]);
    assert!(!out.status.success(), "renaming a missing key must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .to_lowercase()
            .contains("ghost"),
        "the error should name the key"
    );
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
    // Track the binary's own schema version rather than hard-coding it.
    let schema = format!("v{}", llmphant::db::migrate::latest());
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
fn bench_help_lists_its_flags() {
    // `bench` (P10) is a real command now; its flags must be discoverable.
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["bench", "--help"]);
    assert!(
        out.status.success(),
        "bench --help failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    for flag in ["--golden", "--golden-file", "--sweep", "--k"] {
        assert!(text.contains(flag), "help is missing {flag}: {text}");
    }
}

#[test]
fn bench_offline_with_a_cold_cache_names_the_model() {
    // `bench` loads the encoder first, so `--offline` against a throwaway home must fail
    // cleanly in the cache layer (like `embed`), not hang or pretend to run.
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["--offline", "bench"]);
    assert!(
        !out.status.success(),
        "a cold offline bench must not exit 0"
    );
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("offline"), "got: {text}");
    assert!(
        text.contains("Qwen/Qwen3-Embedding-0.6B"),
        "error should name the model: {text}"
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
fn search_surface_is_semantic_only() {
    // The regex leg and the field/match knobs are gone (SPEC §10): `search` takes a phrase and
    // its ranking knobs only.
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["search", "--help"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(!text.contains("--match"), "got: {text}");
    assert!(!text.contains("--field"), "got: {text}");
    assert!(
        text.contains("--k") && text.contains("--max-distance"),
        "got: {text}"
    );
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
