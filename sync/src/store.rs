// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The engine's persistence: the platform data-dir layout and the write
//! discipline every state file follows (doc/sync-engine.md, sections 6 and
//! 9) - temp, fsync, rename, fsync the directory. Rename alone orders the
//! namespace, not the data; a power cut must not hand back a zero-length
//! state file.
//!
//! Layout under the root (in production `<platform data dir>/sync`):
//! `identity.json` (the keypair), `sets/<set_id>/` (one directory per set,
//! populated by the later bricks). The Core stores nothing about sync;
//! `sync.status` is answered entirely from here.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::identity::Identity;
use crate::index::SetIndex;
use crate::records::valid_set_id;

pub struct Store {
    root: PathBuf,
    identity: Identity,
}

impl Store {
    /// Opens (creating if needed) the engine state under `root`, and loads or
    /// mints the identity. An unreadable identity is an error, not a mint:
    /// see the identity module's header. The root is owner-only on unix:
    /// set ids are unguessable capabilities, and a world-listable state
    /// directory would hand them to every local user.
    pub fn open(root: PathBuf) -> io::Result<Store> {
        std::fs::create_dir_all(root.join("sets"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        }
        let identity = Identity::load_or_generate(&root)?;
        Ok(Store { root, identity })
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory one set's files live under, created on first use. The
    /// set_id is re-checked where it becomes a path, whatever parse it
    /// already survived.
    pub fn set_dir(&self, set_id: &str) -> io::Result<PathBuf> {
        if !valid_set_id(set_id) {
            return Err(io::Error::other("invalid set id"));
        }
        let dir = self.root.join("sets").join(set_id);
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Persists one set's meta document (`sets/<set_id>/meta.json`),
    /// atomically. The engine composes the document (membership, root,
    /// pending, watermarks, ...): one file, one rename, so the letter's
    /// absorbed-state-before-watermark ordering is an atomicity property.
    pub fn save_meta(&self, set_id: &str, meta: &Value) -> io::Result<()> {
        let dir = self.set_dir(set_id)?;
        write_private_atomic(&dir.join("meta.json"), meta.to_string().as_bytes())
    }

    /// Persists one set's index as `sets/<set_id>/index.json`.
    pub fn save_index(&self, set_id: &str, index: &SetIndex) -> io::Result<()> {
        let dir = self.set_dir(set_id)?;
        write_private_atomic(
            &dir.join("index.json"),
            index.to_value().to_string().as_bytes(),
        )
    }

    /// Loads a set's index. `None` for an ABSENT file (loud-but-safe: the
    /// lease keeps the clock monotonic and the rescan re-derives the tree);
    /// a present-but-garbled index is an error, never a guess.
    pub fn load_index(&self, set_id: &str) -> io::Result<Option<SetIndex>> {
        let dir = self.set_dir(set_id)?;
        let path = dir.join("index.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let corrupt = || io::Error::other(format!("corrupt {}", path.display()));
        let doc: Value = serde_json::from_str(&text).map_err(|_| corrupt())?;
        Ok(Some(SetIndex::from_value(&doc).ok_or_else(corrupt)?))
    }

    /// Loads every persisted set's meta document, keyed by set_id (the
    /// directory name, re-validated). Corruption is an ERROR, not a shrug:
    /// a set that cannot be reloaded must fail loudly rather than resync
    /// from a guess.
    pub fn load_metas(&self) -> io::Result<Vec<(String, Value)>> {
        let mut sets = Vec::new();
        for entry in std::fs::read_dir(self.root.join("sets"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(set_id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !valid_set_id(&set_id) {
                continue;
            }
            let meta_path = entry.path().join("meta.json");
            let text = match std::fs::read_to_string(&meta_path) {
                Ok(text) => text,
                // A directory without its meta yet (a crash between mkdir
                // and the first write): nothing to load, nothing to lose.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let corrupt = || io::Error::other(format!("corrupt {}", meta_path.display()));
            let meta: Value = serde_json::from_str(&text).map_err(|_| corrupt())?;
            sets.push((set_id, meta));
        }
        Ok(sets)
    }
}

/// Writes `bytes` to `path` atomically and privately: owner-only permissions
/// from birth (unix), then temp, fsync, rename, directory fsync. The temp
/// name lives beside the target in the engine's own data dir, so a crashed
/// leftover is simply overwritten by the next write.
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("no parent directory"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("invalid file name"))?;
    let tmp = dir.join(format!("{name}.tmp"));
    // Remove any leftover first: `mode(0o600)` only applies at creation, and
    // a stale temp with looser permissions must not be inherited.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Directory fsync, unix only: std exposes no directory flush on Windows,
    // where the rename itself is the best available ordering.
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_store_lays_out_its_directories_and_keeps_its_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("state").join("sync");
        let first = Store::open(root.clone()).expect("open");
        assert!(root.join("sets").is_dir());

        let key = first.identity().public_hex();
        drop(first);
        let second = Store::open(root).expect("reopen");
        assert_eq!(second.identity().public_hex(), key);
    }

    #[test]
    fn a_corrupt_identity_keeps_the_store_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("identity.json"), "garbage").expect("write");
        assert!(Store::open(root).is_err());
    }

    #[test]
    fn atomic_writes_replace_and_never_leave_the_temp_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        write_private_atomic(&path, b"first").expect("write");
        write_private_atomic(&path, b"second").expect("overwrite");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");
        assert!(
            !dir.path().join("state.json.tmp").exists(),
            "the temp must have been renamed away"
        );
    }

    #[test]
    fn set_metas_round_trip_through_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().join("sync")).expect("open");
        let meta = json!({ "membership": { "anything": true } });
        store
            .save_meta("AAAAAAAAAAAAAAAAAAAAAA", &meta)
            .expect("save");
        // Idempotent overwrite.
        store
            .save_meta("AAAAAAAAAAAAAAAAAAAAAA", &meta)
            .expect("save again");
        assert!(
            store.save_meta("../escape", &meta).is_err(),
            "a set id is re-checked where it becomes a path"
        );

        let sets = store.load_metas().expect("load");
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0], ("AAAAAAAAAAAAAAAAAAAAAA".to_string(), meta));

        // A corrupt meta fails the LOAD loudly, never silently skips.
        std::fs::write(
            dir.path()
                .join("sync")
                .join("sets")
                .join("AAAAAAAAAAAAAAAAAAAAAA")
                .join("meta.json"),
            "garbage",
        )
        .expect("corrupt");
        assert!(store.load_metas().is_err());
    }

    /// A stale temp file (a crash between create and rename) must not stall
    /// the next write, nor lend it looser permissions.
    #[cfg(unix)]
    #[test]
    fn a_stale_temp_is_replaced_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        let tmp = dir.path().join("state.json.tmp");
        std::fs::write(&tmp, b"leftover").expect("write");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_private_atomic(&path, b"fresh").expect("write");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(std::fs::read(&path).expect("read"), b"fresh");
    }
}
