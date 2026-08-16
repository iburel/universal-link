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

use serde_json::{Value, json};

use crate::identity::Identity;
use crate::membership::SetMembership;
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

    /// The `sync.status` snapshot: the authoritative state the notifications
    /// merely echo. No sets and no invitations yet - the answer is honestly
    /// empty until the membership bricks land.
    pub fn status(&self) -> Value {
        json!({ "sets": [], "invitations": [] })
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Persists one set's state as `sets/<set_id>/meta.json`, atomically.
    /// The membership nests under its own key; later bricks add siblings
    /// (watermarks, the pending set, conflict records) to the same file.
    pub fn save_set(&self, membership: &SetMembership) -> io::Result<()> {
        let set_id = &membership.descriptor.set_id;
        // The descriptor came through the strict parse, but this string is
        // about to name a directory: the invariant is re-checked where it
        // becomes a path.
        if !valid_set_id(set_id) {
            return Err(io::Error::other("invalid set id"));
        }
        let dir = self.root.join("sets").join(set_id);
        std::fs::create_dir_all(&dir)?;
        let meta = json!({ "membership": membership.to_value() });
        write_private_atomic(&dir.join("meta.json"), meta.to_string().as_bytes())
    }

    /// Loads every persisted set. Corruption is an ERROR, not a shrug: a
    /// set whose membership cannot be reloaded must fail loudly rather than
    /// resync from a guess.
    pub fn load_sets(&self) -> io::Result<Vec<SetMembership>> {
        let mut sets = Vec::new();
        for entry in std::fs::read_dir(self.root.join("sets"))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
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
            let membership = meta
                .get("membership")
                .and_then(SetMembership::from_value)
                .ok_or_else(corrupt)?;
            sets.push(membership);
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
    use super::*;

    #[test]
    fn the_store_lays_out_its_directories_and_keeps_its_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("state").join("sync");
        let first = Store::open(root.clone()).expect("open");
        assert!(root.join("sets").is_dir());
        assert_eq!(first.status(), json!({ "sets": [], "invitations": [] }));

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
    fn sets_round_trip_through_the_store() {
        use crate::records::{SetDescriptor, SetKind};

        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().join("sync")).expect("open");
        let descriptor = SetDescriptor::create(
            "AAAAAAAAAAAAAAAAAAAAAA".into(),
            SetKind::Dir,
            "Projects".into(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            1000,
            store.identity(),
        );
        let membership = SetMembership::new(descriptor.clone());
        store.save_set(&membership).expect("save");
        // Idempotent overwrite.
        store.save_set(&membership).expect("save again");

        let sets = store.load_sets().expect("load");
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].descriptor, descriptor);

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
        assert!(store.load_sets().is_err());
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
