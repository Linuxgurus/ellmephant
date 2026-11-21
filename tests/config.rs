mod common;

use common::temp_store;
use ellmephant::config::{Config, Paths};

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
    assert_eq!(cfg.default_k, 5);
    assert_eq!(cfg.instruction, ellmephant::config::DEFAULT_INSTRUCTION);
    assert_eq!(cfg.port, 8888);
    assert_eq!(cfg.port, ellmephant::config::DEFAULT_PORT);
}

#[test]
fn config_toml_overrides_only_the_keys_it_names() {
    let (_dir, paths) = temp_store();
    std::fs::write(
        paths.config(),
        "model = \"local/tiny\"\ndim = 256\noffline = true\ndefault_k = 3\nport = 9999\n",
    )
    .expect("write config");

    let cfg = Config::load(&paths).expect("load overrides");
    assert_eq!(cfg.model, "local/tiny");
    assert_eq!(cfg.dim, 256);
    assert!(cfg.offline);
    assert_eq!(cfg.default_k, 3);
    assert_eq!(cfg.port, 9999);
    assert_eq!(cfg.revision, "main");
    assert_eq!(cfg.device, "cpu");
}

#[test]
fn unknown_and_malformed_config_are_rejected() {
    let (_dir, paths) = temp_store();

    std::fs::write(paths.config(), "modl = \"oops\"\n").expect("write");
    let err = Config::load(&paths).unwrap_err();
    assert!(format!("{err}").contains("toml"), "got: {err}");

    std::fs::write(paths.config(), "dim = = 4\n").expect("write");
    assert!(Config::load(&paths).is_err(), "malformed toml must fail");
}

#[test]
fn validate_rejects_a_bad_device_precision_or_dim() {
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

    let huge_dim = Config {
        dim: ellmephant::config::MAX_DIM + 1,
        ..Config::default()
    };
    let err = huge_dim.validate().unwrap_err();
    assert!(err.to_string().contains("1024"), "{err}");
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

    let explicit = Paths::from_root(root);
    assert_eq!(explicit.root(), root);
}
