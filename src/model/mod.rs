pub mod cache;
pub mod fetch;
pub mod manifest;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{Config, Paths};
use crate::error::{Error, Result};
use crate::model::fetch::{DEFAULT_ENDPOINT, Fetcher};
use crate::model::manifest::{FileEntry, Manifest};

pub const REQUIRED_FILES: &[&str] = &["config.json", "tokenizer.json", "model.safetensors"];
pub const OPTIONAL_FILES: &[&str] = &[
    "tokenizer_config.json",
    "generation_config.json",
    "vocab.json",
    "merges.txt",
];

fn all_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = REQUIRED_FILES.to_vec();
    names.extend_from_slice(OPTIONAL_FILES);
    names
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub repo: String,
    pub revision: String,
    pub cache_root: PathBuf,
    pub model_dir: Option<PathBuf>,
    pub endpoint: String,
    pub offline: bool,
    pub force: bool,
}

impl Spec {
    pub fn from_config(cfg: &Config, paths: &Paths) -> Self {
        let cache_dir = cfg.cache_dir.trim();
        Self {
            repo: cfg.model.clone(),
            revision: cfg.revision.clone(),
            cache_root: if cache_dir.is_empty() {
                paths.models()
            } else {
                PathBuf::from(cache_dir)
            },
            model_dir: None,
            endpoint: std::env::var("HF_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string()),
            offline: cfg.offline,
            force: false,
        }
    }

    pub fn with_overrides(&self, repo: Option<String>, revision: Option<String>) -> Self {
        let mut spec = self.clone();
        if let Some(repo) = repo {
            spec.repo = repo;
        }
        if let Some(revision) = revision {
            spec.revision = revision;
        }
        spec
    }
}

#[derive(Debug, Clone)]
pub struct FileState {
    pub name: String,
    pub present: bool,
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub repo: String,
    pub revision: String,
    pub resolved: Option<String>,
    pub dir: PathBuf,
    pub files: Vec<FileState>,
    pub manifest: bool,
    pub offline: bool,
}

fn looks_like_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_refs(root: &Path, repo: &str) -> Result<BTreeMap<String, String>> {
    let path = cache::refs_path(root, repo);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(&path)?)?)
}

fn record_ref(root: &Path, repo: &str, reference: &str, sha: &str) -> Result<()> {
    let mut refs = read_refs(root, repo)?;
    refs.insert(reference.to_string(), sha.to_string());
    let body = serde_json::to_string_pretty(&refs)?;
    cache::write_atomic(&cache::refs_path(root, repo), body.as_bytes())
}

fn resolve_sha(spec: &Spec, fetcher: Option<&dyn Fetcher>) -> Result<String> {
    if looks_like_sha(&spec.revision) {
        return Ok(spec.revision.clone());
    }
    if let Some(sha) = read_refs(&spec.cache_root, &spec.repo)?.get(&spec.revision) {
        return Ok(sha.clone());
    }
    match fetcher {
        Some(fetcher) if !spec.offline => {
            let url = fetch::revision_api_url(&spec.endpoint, &spec.repo, &spec.revision);
            let sha = fetcher.resolve_ref(&url)?;
            record_ref(&spec.cache_root, &spec.repo, &spec.revision, &sha)?;
            Ok(sha)
        }
        _ => Err(Error::Model(format!(
            "revision {:?} of {} is not resolved and we are offline; run `ellmephant model fetch` \
             once online, or pin a commit sha",
            spec.revision, spec.repo
        ))),
    }
}

pub fn ensure(spec: &Spec, fetcher: &dyn Fetcher) -> Result<PathBuf> {
    if let Some(dir) = &spec.model_dir {
        let missing = missing_required(dir);
        if missing.is_empty() {
            return Ok(dir.clone());
        }
        return Err(Error::Model(format!(
            "--model-dir {} is missing required file(s): {}",
            dir.display(),
            missing.join(", ")
        )));
    }

    let sha = resolve_sha(spec, Some(fetcher))?;
    let dir = cache::revision_dir(&spec.cache_root, &spec.repo, &sha);

    if !spec.force
        && let Some(m) = Manifest::load(&dir)?
        && m.is_complete(&dir, REQUIRED_FILES)
    {
        return Ok(dir);
    }

    if spec.offline {
        return Err(Error::Model(format!(
            "offline and the weights cache is incomplete for {}@{}: {}. \
             Run `ellmephant model fetch` once with network.",
            spec.repo,
            sha,
            missing_required(&dir).join(", ")
        )));
    }

    download_all(spec, fetcher, &dir, &sha)?;
    Ok(dir)
}

pub fn download_all(spec: &Spec, fetcher: &dyn Fetcher, dir: &Path, sha: &str) -> Result<Manifest> {
    std::fs::create_dir_all(dir)?;
    let _lock = cache::Lock::acquire(&dir.join(".lock"))?;

    let mut files = Manifest::load(dir)?.map(|m| m.files).unwrap_or_default();

    for name in all_names() {
        let dest = dir.join(name);
        let mandatory = REQUIRED_FILES.contains(&name);

        if !spec.force
            && let Some(entry) = files.get(name)
            && manifest::size_of(&dest) == Some(entry.size)
        {
            continue;
        }

        let url = fetch::file_url(&spec.endpoint, &spec.repo, sha, name);
        match download_one(fetcher, &url, &dest) {
            Ok(entry) => {
                files.insert(name.to_string(), entry);
            }
            Err(err) if !mandatory => tracing::debug!("skipping optional {name}: {err}"),
            Err(err) => return Err(err),
        }
    }

    let missing: Vec<&str> = REQUIRED_FILES
        .iter()
        .copied()
        .filter(|n| !files.contains_key(*n))
        .collect();
    if !missing.is_empty() {
        return Err(Error::Model(format!(
            "required file(s) missing after fetch: {}",
            missing.join(", ")
        )));
    }

    let manifest = Manifest {
        repo: spec.repo.clone(),
        revision: spec.revision.clone(),
        resolved: sha.to_string(),
        created: now_epoch(),
        files,
    };
    manifest.save(dir)?;
    Ok(manifest)
}

fn download_one(fetcher: &dyn Fetcher, url: &str, dest: &Path) -> Result<FileEntry> {
    let partial = dest.with_extension("partial");
    {
        let mut file = std::fs::File::create(&partial)?;
        let n = fetcher.download(url, &mut file)?;
        file.sync_all()?;
        if n == 0 {
            return Err(Error::Http(format!("empty body from {url}")));
        }
    }
    let sha256 = manifest::sha256_file(&partial)?;
    let size = std::fs::metadata(&partial)?.len();
    std::fs::rename(&partial, dest)?;
    Ok(FileEntry { sha256, size })
}

fn missing_required(dir: &Path) -> Vec<String> {
    REQUIRED_FILES
        .iter()
        .filter(|name| !dir.join(name).is_file())
        .map(|name| (*name).to_string())
        .collect()
}

fn file_states(dir: &Path, names: &[&str]) -> Vec<FileState> {
    names
        .iter()
        .map(|name| {
            let path = dir.join(name);
            FileState {
                name: (*name).to_string(),
                present: path.is_file(),
                size: manifest::size_of(&path),
            }
        })
        .collect()
}

pub fn status(spec: &Spec) -> Result<Status> {
    if let Some(dir) = &spec.model_dir {
        return Ok(Status {
            repo: spec.repo.clone(),
            revision: spec.revision.clone(),
            resolved: None,
            dir: dir.clone(),
            files: file_states(dir, &all_names()),
            manifest: false,
            offline: true,
        });
    }

    let resolved = if looks_like_sha(&spec.revision) {
        Some(spec.revision.clone())
    } else {
        read_refs(&spec.cache_root, &spec.repo)?
            .get(&spec.revision)
            .cloned()
    };
    let dir = match &resolved {
        Some(sha) => cache::revision_dir(&spec.cache_root, &spec.repo, sha),
        None => cache::repo_dir(&spec.cache_root, &spec.repo),
    };
    let manifest = Manifest::load(&dir)?.is_some();

    Ok(Status {
        repo: spec.repo.clone(),
        revision: spec.revision.clone(),
        resolved,
        files: file_states(&dir, &all_names()),
        dir,
        manifest,
        offline: spec.offline,
    })
}

pub fn verify(spec: &Spec) -> Result<Vec<String>> {
    let status = status(spec)?;
    if status.resolved.is_none() && spec.model_dir.is_none() {
        return Err(Error::Model(format!(
            "revision {:?} is not resolved; nothing to verify",
            spec.revision
        )));
    }
    let manifest = Manifest::load(&status.dir)?
        .ok_or_else(|| Error::Model(format!("no manifest in {}", status.dir.display())))?;
    manifest.verify(&status.dir)
}

pub fn remove(spec: &Spec, all: bool) -> Result<usize> {
    if spec.model_dir.is_some() {
        return Err(Error::Model("refusing to remove a --model-dir".into()));
    }
    let repo_dir = cache::repo_dir(&spec.cache_root, &spec.repo);
    if !repo_dir.is_dir() {
        return Ok(0);
    }
    if all {
        remove_all(spec, &repo_dir)
    } else {
        remove_one(spec)
    }
}

fn remove_all(spec: &Spec, repo_dir: &Path) -> Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(repo_dir)? {
        let path = entry?.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
            removed += 1;
        }
    }
    let refs = cache::refs_path(&spec.cache_root, &spec.repo);
    if refs.exists() {
        std::fs::remove_file(refs)?;
    }
    Ok(removed)
}

fn remove_one(spec: &Spec) -> Result<usize> {
    let sha = if looks_like_sha(&spec.revision) {
        Some(spec.revision.clone())
    } else {
        read_refs(&spec.cache_root, &spec.repo)?
            .get(&spec.revision)
            .cloned()
    }
    .ok_or_else(|| {
        Error::Model(format!(
            "revision {:?} is not present in the cache",
            spec.revision
        ))
    })?;

    let dir = cache::revision_dir(&spec.cache_root, &spec.repo, &sha);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir)?;
        return Ok(1);
    }
    Ok(0)
}
