// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! On-disk directory persistence: a JSON snapshot rewritten atomically
//! (temporary file + rename) on every durable mutation.
//!
//! An account's dataset is small; rewriting the complete snapshot is simpler and
//! more robust than incremental storage, and is more than enough for the "one
//! hosted server" model. A real DBMS (SQLite…) would be the next building block
//! if volume or concurrency demanded it.

use std::path::{Path, PathBuf};

use anyhow::Context;
use universallink_server::{DirectoryStore, DurableState};

pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> FileStore {
        FileStore { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl DirectoryStore for FileStore {
    fn load(&self) -> anyhow::Result<DurableState> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a valid directory", self.path.display())),
            // First startup: no file yet, empty directory.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DurableState::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn save(&self, state: &DurableState) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(state).context("serializing the directory")?;
        // Atomic write: a temporary in the SAME folder (hence same filesystem,
        // atomic rename), then a rename onto the target. A crash mid-write never
        // leaves a truncated directory.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming to {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use universallink_server::DurableDevice;

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileStore::new(dir.path().join("directory.json"));

        // Absent on first startup: empty directory, not an error.
        assert!(store.load().expect("empty load").devices.is_empty());

        let state = DurableState {
            devices: vec![DurableDevice {
                account: "alice".into(),
                device_id: "d_1".into(),
                name: "PC".into(),
                platform: "linux".into(),
                node_id: "00".repeat(32),
                attestation: Some("ab".repeat(64)),
            }],
            revoked: vec!["d_old".into()],
        };
        store.save(&state).expect("save");

        // Reloaded by ANOTHER FileStore (simulates a restart).
        let reloaded = FileStore::new(dir.path().join("directory.json"))
            .load()
            .expect("reload");
        assert_eq!(reloaded.devices.len(), 1);
        assert_eq!(reloaded.devices[0].device_id, "d_1");
        assert_eq!(reloaded.devices[0].attestation, Some("ab".repeat(64)));
        assert_eq!(reloaded.revoked, vec!["d_old".to_string()]);
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_an_empty_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("directory.json");
        std::fs::write(&path, "{ not JSON").expect("write");
        // An unreadable directory must not be silently restarted from scratch
        // (that would be a forced OIDC re-login for everyone, masking the fault).
        assert!(FileStore::new(&path).load().is_err());
    }
}
