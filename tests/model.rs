//! Integration: the model cache (SPEC §6) against a **fake transport** — no network.
//!
//! Everything here runs offline; the real `ureq` fetch is exercised only by the
//! `#[ignore]`d test at the bottom.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use llmphant::config::{Config, Paths};
use llmphant::model::fetch::{self, Fetcher};
use llmphant::model::{self, Spec};
use llmphant::{Error, Result};
use tempfile::TempDir;

const REPO: &str = "Qwen/Qwen3-Embedding-0.6B";
const ENDPOINT: &str = "https://hf.test";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// In-memory Hub: url -> bytes, plus url -> sha for ref resolution. Counts downloads.
struct FakeFetcher {
    files: HashMap<String, Vec<u8>>,
    refs: HashMap<String, String>,
    downloads: Cell<usize>,
    fail: HashSet<String>,
}

impl FakeFetcher {
    fn new() -> Self {
        let mut files = HashMap::new();
        for (name, body) in [
            ("config.json", "{\"hidden_size\":1024}"),
            ("tokenizer.json", "{\"model\":\"qwen3\"}"),
            ("tokenizer_config.json", "{\"pad\":\"left\"}"),
            ("model.safetensors", "WEIGHTS-BLOB"),
            // note: vocab.json / merges.txt intentionally absent -> optional skip
        ] {
            files.insert(
                fetch::file_url(ENDPOINT, REPO, SHA, name),
                body.as_bytes().to_vec(),
            );
        }
        let mut refs = HashMap::new();
        refs.insert(
            fetch::revision_api_url(ENDPOINT, REPO, "main"),
            SHA.to_string(),
        );
        Self {
            files,
            refs,
            downloads: Cell::new(0),
            fail: HashSet::new(),
        }
    }

    fn downloads(&self) -> usize {
        self.downloads.get()
    }
}

impl Fetcher for FakeFetcher {
    fn download(&self, url: &str, sink: &mut dyn Write) -> Result<u64> {
        self.downloads.set(self.downloads.get() + 1);
        if self.fail.contains(url) {
            return Err(Error::Http(format!("injected failure for {url}")));
        }
        let body = self
            .files
            .get(url)
            .ok_or_else(|| Error::Http(format!("404 {url}")))?;
        sink.write_all(body)?;
        Ok(body.len() as u64)
    }

    fn resolve_ref(&self, url: &str) -> Result<String> {
        self.refs
            .get(url)
            .cloned()
            .ok_or_else(|| Error::Http(format!("no ref at {url}")))
    }
}

/// A fetcher that fails loudly if the network is used at all.
struct FakeOffline;

impl Fetcher for FakeOffline {
    fn download(&self, url: &str, _sink: &mut dyn Write) -> Result<u64> {
        panic!("network used while offline: {url}");
    }
    fn resolve_ref(&self, url: &str) -> Result<String> {
        panic!("network used while offline: {url}");
    }
}

fn spec(root: &Path) -> Spec {
    Spec {
        repo: REPO.to_string(),
        revision: "main".to_string(),
        cache_root: root.to_path_buf(),
        model_dir: None,
        endpoint: ENDPOINT.to_string(),
        offline: false,
        force: false,
    }
}

fn temp() -> (TempDir, Spec) {
    let dir = tempfile::tempdir().unwrap();
    let spec = spec(dir.path());
    (dir, spec)
}

#[test]
fn fetch_populates_cache_then_reruns_are_noops() {
    let (_dir, spec) = temp();
    let fetcher = FakeFetcher::new();

    let dir = model::ensure(&spec, &fetcher).expect("fetch");
    for name in model::REQUIRED_FILES {
        assert!(dir.join(name).is_file(), "missing {name}");
    }
    assert!(
        dir.join("tokenizer_config.json").is_file(),
        "optional file not fetched"
    );
    assert!(
        !dir.join("vocab.json").exists(),
        "absent optional file should not exist"
    );
    assert!(
        dir.join(model::manifest::FILE).is_file(),
        "manifest not written"
    );

    let manifest = model::manifest::Manifest::load(&dir).unwrap().unwrap();
    assert!(manifest.is_complete(&dir, model::REQUIRED_FILES));
    assert_eq!(manifest.resolved, SHA);

    // A second run downloads nothing.
    let before = fetcher.downloads();
    let again = model::ensure(&spec, &fetcher).expect("second ensure");
    assert_eq!(again, dir);
    assert_eq!(
        fetcher.downloads(),
        before,
        "a warm cache must not re-download"
    );
}

#[test]
fn warm_cache_works_offline() {
    let (_dir, spec) = temp();
    model::ensure(&spec, &FakeFetcher::new()).expect("fetch");

    // Offline, with a fetcher that panics on any use, the cache still resolves.
    let offline = Spec {
        offline: true,
        ..spec.clone()
    };
    let dir = model::ensure(&offline, &FakeOffline).expect("offline ensure");
    assert_eq!(dir, model::cache::revision_dir(&spec.cache_root, REPO, SHA));
}

#[test]
fn cold_cache_offline_fails_honestly() {
    let (_dir, spec) = temp();
    let offline = Spec {
        offline: true,
        ..spec.clone()
    };
    let err = model::ensure(&offline, &FakeOffline).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("offline"), "{msg}");
    assert!(msg.contains("model fetch"), "{msg}");
}

#[test]
fn truncated_file_is_detected_and_repaired() {
    let (_dir, spec) = temp();
    let fetcher = FakeFetcher::new();
    let dir = model::ensure(&spec, &fetcher).expect("fetch");

    std::fs::write(dir.join("model.safetensors"), b"short").unwrap();
    let manifest = model::manifest::Manifest::load(&dir).unwrap().unwrap();
    assert!(
        !manifest.is_complete(&dir, model::REQUIRED_FILES),
        "size check missed it"
    );
    assert_eq!(
        model::verify(&spec).unwrap().len(),
        1,
        "verify missed the corruption"
    );

    model::ensure(&spec, &fetcher).expect("repair");
    assert!(model::verify(&spec).unwrap().is_empty());
}

#[test]
fn remove_drops_the_revision() {
    let (_dir, spec) = temp();
    let dir = model::ensure(&spec, &FakeFetcher::new()).expect("fetch");
    assert_eq!(model::remove(&spec, false).unwrap(), 1);
    assert!(!dir.exists());
    assert_eq!(
        model::remove(&spec, false).unwrap(),
        0,
        "second remove is a no-op"
    );
}

#[test]
fn model_dir_is_used_verbatim_and_offline() {
    let (dir, mut spec) = temp();
    spec.model_dir = Some(dir.path().to_path_buf());

    let err = model::ensure(&spec, &FakeOffline).unwrap_err();
    assert!(err.to_string().contains("config.json"), "{err}");

    for name in model::REQUIRED_FILES {
        std::fs::write(dir.path().join(name), b"x").unwrap();
    }
    let got = model::ensure(&spec, &FakeOffline).expect("model-dir ensure");
    assert_eq!(got, dir.path());

    let status = model::status(&spec).unwrap();
    assert!(status.offline, "--model-dir implies offline");
    assert_eq!(
        status.files.iter().filter(|f| f.present).count(),
        model::REQUIRED_FILES.len()
    );
}

#[test]
fn spec_from_config_defaults_the_cache_to_home_models() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(dir.path());
    let cfg = Config::default();

    let spec = Spec::from_config(&cfg, &paths);
    assert_eq!(spec.repo, cfg.model);
    assert_eq!(spec.revision, cfg.revision);
    assert_eq!(
        spec.cache_root,
        paths.models(),
        "empty cache_dir means <home>/models"
    );
    assert_eq!(spec.model_dir, None);
    assert!(!spec.offline);

    // A configured cache_dir wins; `offline` rides along (SPEC §6.1, §6.4).
    let cfg = Config {
        cache_dir: "/tmp/elsewhere".to_string(),
        offline: true,
        ..cfg
    };
    let spec = Spec::from_config(&cfg, &paths);
    assert_eq!(spec.cache_root, PathBuf::from("/tmp/elsewhere"));
    assert!(spec.offline);
}

#[test]
fn with_overrides_replaces_only_the_supplied_fields() {
    let (_dir, spec) = temp();

    let repo_only = spec.with_overrides(Some("other/repo".into()), None);
    assert_eq!(repo_only.repo, "other/repo");
    assert_eq!(repo_only.revision, spec.revision, "revision untouched");

    let rev_only = spec.with_overrides(None, Some(SHA.into()));
    assert_eq!(rev_only.repo, spec.repo, "repo untouched");
    assert_eq!(rev_only.revision, SHA);

    let unchanged = spec.with_overrides(None, None);
    assert_eq!(unchanged.repo, spec.repo);
    assert_eq!(unchanged.revision, spec.revision);
}

#[test]
fn verify_errors_when_the_revision_never_resolved() {
    // Cold cache, ref `main` unresolved: there is nothing to hash, so verify must say so
    // rather than silently pass (SPEC §6.3).
    let (_dir, spec) = temp();
    let err = model::verify(&spec).unwrap_err();
    assert!(err.to_string().contains("not resolved"), "{err}");
}

#[test]
fn remove_all_clears_every_revision_and_the_refs_index() {
    let (_dir, spec) = temp();
    let dir = model::ensure(&spec, &FakeFetcher::new()).expect("fetch");
    let refs = model::cache::refs_path(&spec.cache_root, REPO);
    assert!(refs.is_file(), "resolving a ref should record it");

    assert_eq!(model::remove(&spec, true).unwrap(), 1);
    assert!(!dir.exists(), "revision dir survives `remove all`");
    assert!(!refs.exists(), "the refs index should be dropped too");
    assert_eq!(
        model::remove(&spec, true).unwrap(),
        0,
        "a second remove is a no-op"
    );
}

#[test]
fn remove_refuses_a_model_dir() {
    let (dir, mut spec) = temp();
    spec.model_dir = Some(dir.path().to_path_buf());
    assert!(
        model::remove(&spec, false).is_err(),
        "--model-dir is not ours to delete"
    );
}

/// Real fetch — needs the network and ~1.2 GB. `cargo test -- --ignored`.
#[test]
#[ignore = "downloads ~1.2 GB from the Hub"]
fn real_fetch_of_qwen3_embedding() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec(dir.path());
    spec.offline = false;
    let fetcher = fetch::UreqFetcher::from_env();
    let out = model::ensure(&spec, &fetcher).expect("real fetch");
    let manifest = model::manifest::Manifest::load(&out).unwrap().unwrap();
    assert!(manifest.is_complete(&out, model::REQUIRED_FILES));
    assert!(model::verify(&spec).unwrap().is_empty());
}
