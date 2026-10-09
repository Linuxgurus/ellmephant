//! Integration: config loading and path resolution (SPEC §12).
//!
//! Runs offline. `Config::load` merges `config.toml` over the defaults; `Paths` owns the
//! store layout. Neither `Config::discover` nor `Paths::discover` is exercised here —
//! both read process-global env vars, so their behaviour is proven through the black-box
//! CLI tests (tests/cli.rs), which set `LLMPHANT_HOME` on a child process.

mod common;

use common::temp_store;
use llmphant::config::{Config, Paths};

#[test]
fn missing_config_file_yields_the_documented_defaults() {
    let (_dir, paths) = temp_store();
    let cfg = Config::load(&paths).expect("load defaults");

    assert_eq!(cfg.model, "Qwen/Qwen3-Embedding-0.6B");
    assert_eq!(cfg.revision, "main");
    assert_eq!(cfg.cache_dir, "");
    assert!(!cfg.offline);
    assert_eq!(cfg.dim, 1024);
    assert_eq!(cfg.precision, "bf16");
    assert_eq!(cfg.device, "cpu");
    assert_eq!(cfg.default_k, 10);
    assert_eq!(cfg.instruction, llmphant::config::DEFAULT_INSTRUCTION);
}

#[test]
fn config_toml_overrides_only_the_keys_it_names() {
    let (_dir, paths) = temp_store();
    std::fs::write(
        paths.config(),
        "model = \"local/tiny\"\ndim = 256\noffline = true\ndefault_k = 3\n",
    )
    .expect("write config");

    let cfg = Config::load(&paths).expect("load overrides");
    assert_eq!(cfg.model, "local/tiny");
    assert_eq!(cfg.dim, 256);
    assert!(cfg.offline);
    assert_eq!(cfg.default_k, 3);
    // Unnamed keys keep their defaults (serde `default`).
    assert_eq!(cfg.revision, "main");
    assert_eq!(cfg.device, "cpu");
}

#[test]
fn unknown_and_malformed_config_are_rejected() {
    let (_dir, paths) = temp_store();

    // `deny_unknown_fields`: a typo must fail loudly, not silently fall back.
    std::fs::write(paths.config(), "modl = \"oops\"\n").expect("write");
    let err = Config::load(&paths).unwrap_err();
    assert!(format!("{err}").contains("toml"), "got: {err}");

    std::fs::write(paths.config(), "dim = = 4\n").expect("write");
    assert!(Config::load(&paths).is_err(), "malformed toml must fail");
}

#[test]
fn validate_rejects_a_bad_device_precision_or_dim() {
    // Defaults are valid, so a bare `validate` is a no-op.
    Config::default().validate().expect("defaults are valid");

    let bad_device = Config {
        device: "gpu".to_string(),
        ..Config::default()
    };
    let err = bad_device.validate().unwrap_err();
    assert!(err.to_string().contains("cpu|metal"), "{err}");

    let bad_precision = Config {
        precision: "int8".to_string(),
        ..Config::default()
    };
    let err = bad_precision.validate().unwrap_err();
    assert!(err.to_string().contains("bf16|f16|f32"), "{err}");

    let zero_dim = Config {
        dim: 0,
        ..Config::default()
    };
    assert!(zero_dim.validate().is_err(), "dim 0 is unusable");
}

#[test]
fn load_rejects_an_invalid_config_file_before_any_model_load() {
    let (_dir, paths) = temp_store();
    std::fs::write(paths.config(), "device = \"tpu\"\n").expect("write");
    let err = Config::load(&paths).unwrap_err();
    assert!(err.to_string().contains("cpu|metal"), "got: {err}");
}

#[test]
fn paths_hang_off_the_root_and_ensure_creates_the_layout() {
    let (_dir, paths) = temp_store();
    let root = paths.root();

    assert_eq!(paths.db(), root.join("memory.db"));
    assert_eq!(paths.config(), root.join("config.toml"));
    assert_eq!(paths.models(), root.join("models"));
    assert_eq!(paths.history(), root.join("history"));

    paths.ensure().expect("ensure");
    assert!(paths.models().is_dir(), "ensure must create models/");

    // `from_root` is verbatim: no hidden `APP_DIR` suffix is appended.
    let explicit = Paths::from_root(root);
    assert_eq!(explicit.root(), root);
}
