use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub fn repo_slug(repo: &str) -> String {
    repo.trim_matches('/').replace('/', "--")
}

pub fn repo_dir(root: &Path, repo: &str) -> PathBuf {
    root.join(repo_slug(repo))
}

pub fn revision_dir(root: &Path, repo: &str, sha: &str) -> PathBuf {
    repo_dir(root, repo).join(sha)
}

pub fn refs_path(root: &Path, repo: &str) -> PathBuf {
    repo_dir(root, repo).join("refs.json")
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Model(format!("no parent directory for {}", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub struct Lock {
    file: File,
}

impl Lock {
    pub fn acquire(path: &Path) -> Result<Lock> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        lock_exclusive(&file)?;
        Ok(Lock { file })
    }
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &File) -> Result<()> {
    Ok(())
}

impl Drop for Lock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            use std::os::unix::io::AsRawFd;
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_split_on_slash_only() {
        assert_eq!(
            repo_slug("Qwen/Qwen3-Embedding-0.6B"),
            "Qwen--Qwen3-Embedding-0.6B"
        );
        assert_eq!(repo_slug("/org/name/"), "org--name");
        assert_eq!(repo_slug("bare"), "bare");
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!path.with_extension("tmp").exists(), "temp left behind");
    }

    #[test]
    fn a_lock_is_released_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".lock");
        let first = Lock::acquire(&path).unwrap();
        drop(first);
        Lock::acquire(&path).unwrap();
    }
}
