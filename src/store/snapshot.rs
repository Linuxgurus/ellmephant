use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, params};

use crate::error::Result;

pub fn dir(root: &Path) -> PathBuf {
    root.join("snapshot")
}

pub fn file(root: &Path) -> PathBuf {
    dir(root).join("memory.db")
}

pub fn write(conn: &Connection, root: &Path) -> Result<PathBuf> {
    let d = dir(root);
    std::fs::create_dir_all(&d)?;
    let tmp = d.join(format!(".memory.db.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    conn.execute("VACUUM INTO ?1", params![tmp.to_string_lossy()])?;
    let out = file(root);
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

pub fn age_seconds(root: &Path) -> Option<i64> {
    let mtime = std::fs::metadata(file(root)).ok()?.modified().ok()?;
    mtime.elapsed().ok().map(|d| d.as_secs() as i64)
}

pub fn every() -> Option<Duration> {
    let hours: u64 = std::env::var("ELLMEPHANT_SNAPSHOT_HOURS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(6);
    (hours > 0).then(|| Duration::from_secs(hours * 3600))
}
