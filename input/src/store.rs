// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The engine's persistence (doc/input-sharing.md, section 11): the platform
//! data-dir layout and the write discipline each state file follows.
//!
//! Layout under the root (in production `<platform data dir>/input`):
//!
//! - `identity.json`: the engine keypair. Owner-only, atomic, fsynced.
//! - `plane.json`: the merged layout document plus the pinned engine keys of the
//!   peers. Atomic and fsynced: a half-written plane would put screens in the
//!   wrong places, and a plane that disagrees with a sibling's refuses every
//!   session until a round repairs it.
//! - `settings.json`: the grants (who may drive this computer), the outbound
//!   enablements and their modes, the per neighbour guards, the incoming
//!   modifier remappings, the return hotkey, the lock toggle. Atomic and
//!   fsynced: this file holds the feature's security boundary, and a grant that
//!   survived a crash in a half-written state is not a state anyone can reason
//!   about.
//! - `held.json`: the stuck-key crash guard, and the ONE file written WITHOUT
//!   fsync. See [`Store::save_held`] for why that is the right call rather than
//!   a shortcut.
//!
//! # A corruption policy PER FILE, because they are not the same kind of thing
//!
//! One rule for all four was wrong in both directions, and the two files it was
//! wrong about are the two that most need reading (doc/input-sharing.md, section
//! 11):
//!
//! - `identity.json` is **fatal**: a silently fresh key unpins this device
//!   everywhere. Report and stay down, visibly, until a human decides.
//! - `settings.json` is **fatal**: it holds the permissions, and starting fresh
//!   over an unreadable one would silently re-open a door somebody closed or
//!   close one they opened.
//! - `plane.json` is **lenient** ([`Store::load_plane`]): a plane is rebuilt by
//!   one round with any peer, so refusing to start over a file that will be
//!   replaced in seconds is strictness with no payoff.
//! - `held.json` is **lenient** ([`Store::load_held`]), and this one is the
//!   sharpest of the four: it is the file written WITHOUT fsync, so it is
//!   precisely the one a power cut leaves zero-length or torn, and it is the
//!   CRASH GUARD. Treating it as fatal made a corrupt guard the reason the guard
//!   could not run, which is the opposite of what the file is for.
//!
//! The Core stores nothing; `input.status` is answered entirely from here plus
//! the live session.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::identity::Identity;

pub const PLANE_FILE: &str = "plane.json";
pub const SETTINGS_FILE: &str = "settings.json";
pub const HELD_FILE: &str = "held.json";

pub struct Store {
    root: PathBuf,
    identity: Identity,
}

impl Store {
    /// Opens (creating if needed) the engine state under `root`, and loads or
    /// mints the identity. An unreadable identity is an error, not a mint: see
    /// the identity module's header. The root is owner-only on unix, because
    /// `settings.json` says which of the account's computers may type on this
    /// one, and that is nobody else's business on a shared machine.
    pub fn open(root: PathBuf) -> io::Result<Store> {
        std::fs::create_dir_all(&root)?;
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

    /// Persists the layout document and the pins.
    pub fn save_plane(&self, plane: &Value) -> io::Result<()> {
        write_private_atomic(&self.root.join(PLANE_FILE), plane.to_string().as_bytes())
    }

    /// Persists the grants, the guards, the remappings and the toggles.
    pub fn save_settings(&self, settings: &Value) -> io::Result<()> {
        write_private_atomic(
            &self.root.join(SETTINGS_FILE),
            settings.to_string().as_bytes(),
        )
    }

    /// Loads a state document the FATAL way: `None` when the file is ABSENT (a
    /// first start, or a state this device never had), an ERROR when it is present
    /// and garbled.
    ///
    /// The asymmetry is the house rule and it matters here: an absent settings
    /// file is a device nobody has granted anything on, while a corrupt one is a
    /// device whose grants said something this engine can no longer read, and
    /// starting fresh over that would silently re-open a door or silently close
    /// one. Loud beats convenient.
    ///
    /// This is the right rule for `settings.json` and the wrong one for the other
    /// two state files: see [`Store::load_plane`] and [`Store::load_held`], and the
    /// per-file policy in this module's header.
    pub fn load(&self, file: &str) -> io::Result<Option<Value>> {
        let Some(bytes) = self.read_bytes(file)? else {
            return Ok(None);
        };
        let doc: Value = serde_json::from_slice(&bytes)
            .map_err(|_| io::Error::other(format!("corrupt {}", self.root.join(file).display())))?;
        Ok(Some(doc))
    }

    /// The bytes of a state file, `None` when it is absent. Reading and PARSING
    /// are separate steps on purpose: the per-file policy above turns on telling
    /// "this store cannot be read at all" from "this document is garbled", and a
    /// torn file is not even valid UTF-8, so the two failures have to be
    /// distinguishable rather than both arriving as one `io::Error`.
    fn read_bytes(&self, file: &str) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.root.join(file)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Loads the layout document LENIENTLY: an unreadable `plane.json` is `None`
    /// and a warning, never an error.
    ///
    /// [`crate::plane::Plane::from_stored`] has always promised this ("anything
    /// unreadable yields an empty plane rather than an error") and could not
    /// deliver it, because the load in front of it refused first and the component
    /// stayed down over a file that one round with any peer rebuilds. The plane
    /// authorizes nothing, which is what makes the leniency safe: unlike the
    /// grants, nothing here is a permission, so guessing "empty" cannot open a
    /// door.
    ///
    /// An `Err` still comes back for a file that is present and cannot be READ
    /// (permissions, a failing disk): that is not a corrupt document, it is a
    /// store this engine does not have, and it will not be able to save either.
    pub fn load_plane(&self) -> io::Result<Option<Value>> {
        let Some(bytes) = self.read_bytes(PLANE_FILE)? else {
            return Ok(None);
        };
        match serde_json::from_slice(&bytes) {
            Ok(doc) => Ok(Some(doc)),
            Err(_) => {
                eprintln!(
                    "[1device-input] {PLANE_FILE} is not a readable layout document: starting \
                     from an empty plane, which one round with any peer refills"
                );
                Ok(None)
            }
        }
    }

    /// Loads the stuck-key crash guard LENIENTLY: anything unreadable is
    /// `Value::Null`, which [`crate::keys`] reads as "nothing was held".
    ///
    /// Never an error, and never a reason not to start, because of what this file
    /// is: it is the one written WITHOUT fsync (see [`Store::save_held`]), so it is
    /// exactly the file a power cut leaves zero-length or torn, and it is the guard
    /// against a machine left with Control held down. A guard whose own file can
    /// stop it from running is not a guard. The content is tolerated by
    /// `keys::Held::from_value`, which could never be reached while the load in
    /// front of it failed first.
    pub fn load_held(&self) -> Value {
        let bytes = match self.read_bytes(HELD_FILE) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Value::Null,
            Err(_) => {
                eprintln!(
                    "[1device-input] {HELD_FILE} could not be read: assuming nothing was held"
                );
                return Value::Null;
            }
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            eprintln!(
                "[1device-input] {HELD_FILE} was torn or garbled: assuming nothing was held, \
                 which is the only answer that lets this device start at all"
            );
            Value::Null
        })
    }

    /// Records the modifier keys this engine is holding down on the OS, BEFORE
    /// it presses one.
    ///
    /// This is the stuck-key crash guard (doc/input-sharing.md, section 8 and
    /// D17). An injected key stays down after the injector exits, and a machine
    /// left with Control held is the classic failure of every tool in this
    /// category, so the set is on disk before the press and drained with a
    /// release-all at the next start.
    ///
    /// **Written without fsync, on purpose.** The failure guarded against is a
    /// PROCESS death, and the page cache survives that; a machine that loses
    /// power releases every key by rebooting. Paying an fsync here would put
    /// milliseconds of disk latency on the path of pressing Shift, to buy
    /// durability against a failure that repairs itself.
    ///
    /// **Modifiers only**, and that too is deliberate: an ordinary character key
    /// is down for milliseconds, so the window a crash could strand one in is
    /// negligible and its damage is one repeated character, where a modifier is
    /// down for seconds and its damage is a machine with a dead keyboard.
    /// Writing per character would put a file write on the typing path for no
    /// gain.
    pub fn save_held(&self, held: &Value) -> io::Result<()> {
        let path = self.root.join(HELD_FILE);
        let tmp = self.root.join(format!("{HELD_FILE}.tmp"));
        // Same private-from-birth care as the atomic writer, minus the two
        // fsyncs: the rename still makes a reader see one whole file or the
        // other, which is all this guard needs.
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
            file.write_all(held.to_string().as_bytes())?;
        }
        std::fs::rename(&tmp, &path)
    }
}

/// Writes `bytes` to `path` atomically and privately: owner-only permissions
/// from birth (unix), then temp, fsync, rename, directory fsync. Rename alone
/// orders the namespace, not the data; a power cut must not hand back a
/// zero-length state file. The temp name lives beside the target in the engine's
/// own data dir, so a crashed leftover is simply overwritten by the next write.
///
/// A second copy of the sync engine's writer (`sync/src/store.rs`), and it is a
/// copy on purpose: the two components share no crate, and lifting one function
/// into a shared one would couple two independently replaceable components to
/// each other for eleven lines.
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("no parent directory"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("invalid file name"))?;
    let tmp = dir.join(format!("{name}.tmp"));
    // Remove any leftover first: `mode(0o600)` only applies at creation, and a
    // stale temp with looser permissions must not be inherited.
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
    fn state_round_trips_and_an_absent_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().join("input")).expect("open");
        assert_eq!(store.load(PLANE_FILE).expect("absent is fine"), None);
        assert_eq!(store.load(SETTINGS_FILE).expect("absent is fine"), None);

        let plane = json!({ "placement": { "seq": 3 } });
        store.save_plane(&plane).expect("save");
        assert_eq!(store.load(PLANE_FILE).expect("load"), Some(plane));

        let settings = json!({ "allow": { "abc": true } });
        store.save_settings(&settings).expect("save");
        assert_eq!(store.load(SETTINGS_FILE).expect("load"), Some(settings));
    }

    /// A corrupt state file is loud. Silently starting fresh would re-open a
    /// door the owner had closed, or close one they had opened, and neither is
    /// something to guess at.
    #[test]
    fn a_corrupt_state_file_is_an_error_not_a_fresh_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().to_path_buf()).expect("open");
        std::fs::write(dir.path().join(SETTINGS_FILE), "{ not json").expect("write");
        assert!(store.load(SETTINGS_FILE).is_err());
    }

    /// The held file is written and re-read like the rest; the only thing it
    /// gives up is the fsync, which is invisible from here and is the point.
    #[test]
    fn the_held_set_survives_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().to_path_buf()).expect("open");
        let held = json!({ "keys": [{ "code": 16, "detail": 0 }] });
        store.save_held(&held).expect("save");
        assert_eq!(store.load(HELD_FILE).expect("load"), Some(held));
        // And emptying it is a write, not a delete: a reader must be able to
        // tell "nothing is held" from "this file was never written".
        store.save_held(&json!({ "keys": [] })).expect("save empty");
        assert_eq!(
            store.load(HELD_FILE).expect("load"),
            Some(json!({ "keys": [] }))
        );
        assert_eq!(store.load_held(), json!({ "keys": [] }));
    }

    /// A corrupt `plane.json` is NOT a reason to stay down: the plane authorizes
    /// nothing and one round with any peer rebuilds it, so refusing to start over a
    /// file that will be replaced in seconds is strictness with no payoff.
    /// `Plane::from_stored` promised exactly this and could not deliver it while the
    /// load in front of it refused first.
    #[test]
    fn a_corrupt_plane_is_not_a_reason_for_the_component_to_stay_down() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().to_path_buf()).expect("open");
        assert_eq!(store.load_plane().expect("absent is fine"), None);

        let plane = json!({ "monitors": {}, "placement": { "seq": 3 } });
        store.save_plane(&plane).expect("save");
        assert_eq!(store.load_plane().expect("load"), Some(plane));

        for torn in [
            b"{ \"monitors\":".to_vec(),
            Vec::new(),
            vec![0xff, 0xfe, 0x00],
        ] {
            std::fs::write(dir.path().join(PLANE_FILE), &torn).expect("write");
            assert_eq!(
                store.load_plane().expect("a torn plane is not an error"),
                None,
                "an unreadable plane starts empty, whatever shape the damage took"
            );
        }
        // And the settings, which ARE permissions, stay fatal in the same store.
        std::fs::write(dir.path().join(SETTINGS_FILE), "{ not json").expect("write");
        assert!(
            store.load(SETTINGS_FILE).is_err(),
            "the leniency is per file and not a new house rule"
        );
    }

    /// A torn `held.json` is not a reason the crash guard cannot run, which is the
    /// sharpest of the four policies: this is the ONE file written without fsync, so
    /// it is exactly the file a power cut leaves zero-length or garbled, and it is
    /// the thing that releases a modifier the engine died holding. Treating it as
    /// fatal made a corrupt guard the reason the guard could not run.
    #[test]
    fn a_torn_held_file_is_not_a_reason_the_crash_guard_cannot_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().to_path_buf()).expect("open");
        assert_eq!(
            store.load_held(),
            Value::Null,
            "an absent guard file is nothing held"
        );
        for torn in [
            b"{ \"keys\": [{ \"code\"".to_vec(),
            Vec::new(),
            vec![0x00; 16],
            b"\xff\xfe garbage".to_vec(),
        ] {
            std::fs::write(dir.path().join(HELD_FILE), &torn).expect("write");
            assert_eq!(
                store.load_held(),
                Value::Null,
                "the guard reads 'nothing was held' rather than refusing to start"
            );
        }
        // The content itself is `keys::Held::from_value`'s business, and it is
        // tolerant of anything: what mattered was being able to reach it at all.
        std::fs::write(dir.path().join(HELD_FILE), "{ \"keys\": \"soon\" }").expect("write");
        assert_eq!(store.load_held(), json!({ "keys": "soon" }));
    }

    #[cfg(unix)]
    #[test]
    fn the_state_directory_and_its_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("input");
        let store = Store::open(root.clone()).expect("open");
        store.save_settings(&json!({})).expect("save");
        store.save_held(&json!({})).expect("save");
        let mode =
            |p: PathBuf| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(root.clone()), 0o700);
        assert_eq!(mode(root.join(SETTINGS_FILE)), 0o600);
        assert_eq!(mode(root.join(HELD_FILE)), 0o600);
    }
}
