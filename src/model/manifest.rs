use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;

pub const FILE: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub repo: String,
    pub revision: String,
    pub resolved: String,
    pub created: u64,
    pub files: BTreeMap<String, FileEntry>,
}

impl Manifest {
    pub fn load(dir: &Path) -> Result<Option<Manifest>> {
        let path = dir.join(FILE);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        crate::model::cache::write_atomic(&dir.join(FILE), text.as_bytes())
    }

    pub fn is_complete(&self, dir: &Path, required: &[&str]) -> bool {
        required.iter().all(|name| {
            self.files
                .get(*name)
                .is_some_and(|e| size_of(&dir.join(name)) == Some(e.size))
        })
    }

    pub fn verify(&self, dir: &Path) -> Result<Vec<String>> {
        let mut bad = Vec::new();
        for (name, entry) in &self.files {
            match sha256_file(&dir.join(name)) {
                Ok(got) if got == entry.sha256 => {}
                Ok(got) => bad.push(format!("{name}: sha256 {got} != {}", entry.sha256)),
                Err(err) => bad.push(format!("{name}: {err}")),
            }
        }
        Ok(bad)
    }
}

pub fn size_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut reader = BufReader::new(File::open(path)?);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub use crate::hex::encode as hex;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_file_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn save_load_round_trip_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), b"{}").unwrap();
        let mut files = BTreeMap::new();
        files.insert(
            "config.json".to_string(),
            FileEntry {
                sha256: sha256_file(&dir.path().join("config.json")).unwrap(),
                size: 2,
            },
        );
        let m = Manifest {
            repo: "o/r".into(),
            revision: "main".into(),
            resolved: "abc".into(),
            created: 1,
            files,
        };
        m.save(dir.path()).unwrap();

        let back = Manifest::load(dir.path()).unwrap().unwrap();
        assert!(back.is_complete(dir.path(), &["config.json"]));
        assert!(back.verify(dir.path()).unwrap().is_empty());

        std::fs::write(dir.path().join("config.json"), b"{}extra").unwrap();
        assert!(!back.is_complete(dir.path(), &["config.json"]));
        assert_eq!(back.verify(dir.path()).unwrap().len(), 1);
    }
}
