use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{BundleError, FORMAT, Manifest, inspect, sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexEntry {
    pub matrix_version: String,
    pub published_at: DateTime<Utc>,
    pub format: String,
    pub file: String,
    pub size: u64,
    pub sha256: String,
}

pub fn filename(published_at: DateTime<Utc>) -> String {
    format!("{FORMAT}-{}.tar", published_at.format("%Y%m%dT%H%M%S%.9fZ"))
}

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn temporary(dir: &Path, name: &str, bytes: &[u8]) -> Result<TempFile, BundleError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let path = dir.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let temp = TempFile(path);
                file.write_all(bytes)?;
                file.sync_all()?;
                return Ok(temp);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn entry(path: &Path, bytes: &[u8]) -> Result<IndexEntry, BundleError> {
    let manifest = inspect(bytes)?.manifest;
    let file = filename(manifest.published_at);
    if path.file_name().and_then(|p| p.to_str()) != Some(file.as_str()) {
        return Err(BundleError::Invalid(
            "bundle file name does not match manifest",
        ));
    }
    Ok(IndexEntry {
        matrix_version: manifest.matrix_version,
        published_at: manifest.published_at,
        format: manifest.format,
        file,
        size: bytes.len() as u64,
        sha256: sha256(bytes),
    })
}

/// The caller serializes publishers (the export job holds a Postgres advisory lock).
/// A crash between the immutable file and index writes is repaired on the next call.
pub fn publish(dir: &Path, bytes: &[u8]) -> Result<IndexEntry, BundleError> {
    fs::create_dir_all(dir)?;
    let Manifest {
        published_at,
        matrix_version,
        ..
    } = inspect(bytes)?.manifest;
    let name = filename(published_at);
    let path = dir.join(&name);
    let temp = temporary(dir, &name, bytes)?;
    match fs::hard_link(&temp.0, &path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let stored = entry(&path, &fs::read(&path)?)?;
    if stored.matrix_version != matrix_version || stored.published_at != published_at {
        return Err(BundleError::Invalid("immutable bundle identity collision"));
    }
    let mut index = Vec::new();
    for file in fs::read_dir(dir)? {
        let file = file?;
        let name = file.file_name();
        let name = name.to_string_lossy();
        if file.file_type()?.is_file() && name.starts_with("mmbundle1-") && name.ends_with(".tar") {
            index.push(entry(&file.path(), &fs::read(file.path())?)?);
        }
    }
    index.sort_by(|a, b| {
        b.published_at
            .cmp(&a.published_at)
            .then_with(|| a.file.cmp(&b.file))
    });
    let temp_index = temporary(dir, "index.json", &serde_json::to_vec(&index)?)?;
    fs::rename(&temp_index.0, dir.join("index.json"))?;
    fs::File::open(dir)?.sync_all()?;
    Ok(stored)
}
