use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store::SnapshotRecord;

const SNAPSHOT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct SnapshotFile {
    version: u32,
    records: Vec<SnapshotRecord>,
}

pub(crate) fn load(path: &Path) -> Vec<SnapshotRecord> {
    let Ok(raw) = fs::read(path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<SnapshotFile>(&raw) {
        Ok(file) if file.version == SNAPSHOT_VERSION => file.records,
        _ => Vec::new(),
    }
}

pub(crate) fn store(path: &Path, records: &[SnapshotRecord]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(&SnapshotFile {
        version: SNAPSHOT_VERSION,
        records: records.to_vec(),
    })
    .map_err(io::Error::other)?;
    // Rename, never truncate-in-place: a crash mid-write must leave the
    // previous snapshot intact rather than a half-parsed file.
    let temp = temp_path(path);
    fs::write(&temp, &body)?;
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&temp);
            Err(err)
        }
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}
