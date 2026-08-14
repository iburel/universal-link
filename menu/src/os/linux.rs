// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Linux family-A surfaces: two artifacts under `$XDG_DATA_HOME`.
//!
//! - a **KDE ServiceMenu** for Dolphin ([`ServiceMenu`]) — one `.desktop` file
//!   holding one `[Desktop Action …]` per target. KIO rebuilds its list from
//!   `$XDG_DATA_HOME/kio/servicemenus/*.desktop` every time it opens a context
//!   menu (`KFileItemActionsPrivate::serviceMenuFilePaths`), so a rewrite takes
//!   effect on the next right click: no cache to rebuild, nothing to notify.
//!   Needs KIO 5.85 or newer (Plasma 5.23, October 2021) — that release is what
//!   introduced this directory, replacing `kservices5/ServiceMenus` and its
//!   `kbuildsycoca5` dance. Older desktops simply see no entry, which is the
//!   fail-closed side to be on.
//! - **Nautilus scripts** ([`Scripts`]) — one executable script per target in
//!   `$XDG_DATA_HOME/nautilus/scripts/1Device/`. Nautilus turns a
//!   subdirectory into a submenu and takes each file's NAME as the label
//!   (`update_directory_in_scripts_menu`), which is the whole reason that surface
//!   has to sanitize and deduplicate: a label is a file name there, and two PCs
//!   called "PC" must not collapse onto one entry.
//!
//! # Deliberate choices
//!
//! **Both are written unconditionally**, whether or not the matching file manager
//! is installed — the artifacts are a few hundred bytes each, and nothing reads
//! the one whose reader is absent. Detecting the desktop would trade that for a
//! feature that silently does not exist when the guess is wrong (a Nautilus user
//! on a KDE session, a file manager installed after us, `$XDG_CURRENT_DESKTOP`
//! unset under a bare window manager).
//!
//! **Nothing we did not write is ever deleted.** Both directories are enumerated
//! and pruned, which is the only way an absolute surface can drop an entry whose
//! device is gone; every artifact therefore carries [`MARKER`] and the pruning
//! skips anything without it.
//!
//! **The marker is the authority, the container is the scope** ([`sweep`]). A
//! surface removes marked files by enumerating the whole directory its reader
//! reads, not by unlinking the names this version writes — so an artifact left by
//! a version that named things differently is swept on the next startup instead of
//! sitting in a menu for ever. KIO makes the point plainest: it deduplicates
//! service menus by FILE NAME, so renaming ours would leave the old file being
//! read on every right click.
//!
//! **An identical write is skipped.** Nautilus watches the scripts directory and
//! rebuilds its menu when it changes, and the orchestrator re-applies the current
//! list at startup and after any failure. Rewriting the same bytes would churn a
//! menu that may be open.
//!
//! # Known ceiling
//!
//! Nautilus shows at most `TEMPLATE_LIMIT` = 30 entries per directory. Past 30
//! online devices the tail is silently dropped by Nautilus itself — the targets
//! arrive sorted by label, so it is the end of the alphabet that goes. Dolphin has
//! no such limit.

mod scripts;
mod servicemenu;

use std::collections::HashSet;
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub use scripts::Scripts;
pub use servicemenu::ServiceMenu;

use crate::surface::{HelperCommand, MenuSurface};

/// Marks a file as ours, on a line of its own in every artifact we write.
///
/// Load-bearing: [`Scripts`] enumerates a directory and deletes what is no longer
/// wanted, and mistaking a file someone else put there for a stale entry of ours
/// would destroy their work.
pub(crate) const MARKER: &str = "1device-menu:generated";

/// Name of the temporary file the atomic writes go through. Hidden on purpose:
/// Nautilus skips dotfiles, so a crash between the write and the rename cannot
/// leave a bogus entry in a menu.
const TMP_NAME: &str = ".1device-menu.tmp";

/// Both Linux surfaces, rooted at `data_home`. They render the same list and are
/// independent: a broken KDE install does not cost the Nautilus entries.
pub fn surfaces(data_home: &Path, helper: HelperCommand) -> Vec<Box<dyn MenuSurface>> {
    vec![
        Box::new(ServiceMenu::new(data_home, helper.clone())),
        Box::new(Scripts::new(data_home, helper)),
    ]
}

/// `$XDG_DATA_HOME`, or the base directory spec's default for it.
pub fn data_home() -> Option<PathBuf> {
    data_home_from(&|key| std::env::var_os(key))
}

/// Same, with the environment injected. `std::env::set_var` is unsafe (and
/// process-wide) in edition 2024, so the rules are tested this way rather than by
/// mutating the test process.
fn data_home_from(get: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    // The base directory spec: a value that is unset, empty, or not absolute is
    // invalid and the default applies.
    if let Some(dir) = get("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
    {
        return Some(dir);
    }
    let home = get("HOME").map(PathBuf::from).filter(|h| h.is_absolute())?;
    Some(home.join(".local").join("share"))
}

/// Writes `content` at `path` with mode `mode`, unless the file is already
/// exactly that, creating the parent directory if needed.
///
/// Atomic: a temporary file in the same directory, then `rename(2)`. A reader
/// therefore never parses half an entry — and a file manager that opens the
/// context menu mid-write sees either the old list or the new one.
pub(crate) fn write_if_changed(path: &Path, content: &str, mode: u32) -> io::Result<()> {
    if unchanged(path, content, mode) {
        return Ok(());
    }
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no directory to write {} into", path.display()),
        )
    })?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(TMP_NAME);
    let written = std::fs::write(&tmp, content)
        .and_then(|()| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)))
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        // Leaving it behind would be harmless (it is hidden, and pruned as one of
        // ours on the next pass) but there is no reason to.
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Whether `path` already holds exactly this, mode included. The mode is part of
/// it: both artifacts must stay executable, and identical content with the bit
/// lost is an entry the file manager ignores.
fn unchanged(path: &Path, content: &str, mode: u32) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.permissions().mode() & 0o777 == mode
        && std::fs::read(path).is_ok_and(|bytes| bytes == content.as_bytes())
}

/// Removes `path` if it is there. Absent is success: `apply(&[])` runs at every
/// startup, when there is usually nothing left to remove.
pub(crate) fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Whether `path` is one of ours, i.e. carries [`MARKER`].
///
/// The read is bounded: this runs on whatever a stranger may have left in the
/// scripts directory, and the marker is on the second line of everything we
/// write.
pub(crate) fn is_ours(path: &Path) -> bool {
    use std::io::Read;

    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Vec::new();
    if file.take(4096).read_to_end(&mut head).is_err() {
        return false;
    }
    head.windows(MARKER.len()).any(|w| w == MARKER.as_bytes())
}

/// What one directory's sweep left behind.
pub(crate) struct Swept {
    /// Entries that stayed: files that are not ours, and directories.
    pub(crate) left: usize,
}

/// Removes every file directly in `dir` that carries [`MARKER`] and whose name is
/// not in `keep`.
///
/// The scope is the CONTAINER, not the names this version happens to write. Both
/// file managers read every file in the directory they look at, so an artifact of
/// ours under a name we no longer write is not dormant — it is a live menu entry,
/// frozen on the device list of the day it was written, whose clicks the manager
/// then refuses (`NO_SUCH_TARGET`) because they name devices that may be long
/// gone. Sweeping by marker rather than by expected name is what makes renaming an
/// artifact self-healing on upgrade, and it is why no list of retired names has to
/// be kept in step with the code — the kind of list nothing cross-checks.
///
/// Directories are counted and never removed. A directory carries no marker, and
/// deciding it is ours from what is inside it would eventually delete a folder of
/// the user's own scripts.
pub(crate) fn sweep(dir: &Path, keep: &HashSet<&str>) -> io::Result<Swept> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // Nothing was ever written here, or the directory is already gone.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Swept { left: 0 }),
        Err(e) => return Err(e),
    };
    let mut swept = Swept { left: 0 };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| keep.contains(name)) {
            swept.left += 1;
            continue;
        }
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_file()) && is_ours(&path) {
            remove_if_present(&path)?;
        } else {
            swept.left += 1;
        }
    }
    Ok(swept)
}

/// The label a surface shows for `name`, trimmed.
///
/// Both surfaces trim, and for the same reason: a Desktop Entry value and a file
/// name both lose their surrounding whitespace on the way in (KConfig strips it
/// around `=`), so keeping it would mean the label we write and the label the OS
/// shows disagree — and every "did this change?" comparison downstream would be
/// wrong about it.
pub(crate) fn label_of(name: &str) -> &str {
    name.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'static {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| {
            owned
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn the_data_home_is_the_xdg_one_when_it_is_absolute() {
        let get = env_of(&[
            ("XDG_DATA_HOME", "/run/user/1000/share"),
            ("HOME", "/home/i"),
        ]);
        assert_eq!(
            data_home_from(&get),
            Some(PathBuf::from("/run/user/1000/share"))
        );
    }

    /// The spec: unset, empty or relative are all invalid, and the default
    /// applies. Honouring a relative one would scatter menu entries wherever the
    /// Core happened to be started from.
    #[test]
    fn an_invalid_xdg_data_home_falls_back_to_the_default() {
        for bad in ["", ".local/share", "~/share"] {
            let get = env_of(&[("XDG_DATA_HOME", bad), ("HOME", "/home/i")]);
            assert_eq!(
                data_home_from(&get),
                Some(PathBuf::from("/home/i/.local/share")),
                "XDG_DATA_HOME={bad:?} should have been ignored"
            );
        }
        let get = env_of(&[("HOME", "/home/i")]);
        assert_eq!(
            data_home_from(&get),
            Some(PathBuf::from("/home/i/.local/share"))
        );
    }

    #[test]
    fn without_a_usable_home_there_is_nowhere_to_write() {
        assert_eq!(data_home_from(&env_of(&[])), None);
        assert_eq!(data_home_from(&env_of(&[("HOME", "relative")])), None);
    }

    #[test]
    fn an_identical_write_is_skipped_but_a_different_one_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub").join("artifact");

        write_if_changed(&path, "one", 0o755).expect("first write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "one");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o755);

        // An identical write must not touch the file: Nautilus watches the
        // directory and would rebuild an open menu for nothing.
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("set mtime");
        write_if_changed(&path, "one", 0o755).expect("second write");
        assert_eq!(
            std::fs::metadata(&path).expect("meta").modified().ok(),
            Some(old),
            "an unchanged artifact was rewritten"
        );

        write_if_changed(&path, "two", 0o755).expect("third write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "two");
    }

    /// Same bytes, lost executable bit: the file manager ignores a script that is
    /// not executable, so this has to count as a change.
    #[test]
    fn a_wrong_mode_is_repaired_even_when_the_content_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("artifact");
        write_if_changed(&path, "one", 0o755).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        write_if_changed(&path, "one", 0o755).expect("rewrite");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    /// The atomic write leaves nothing behind: a leftover temporary file in the
    /// scripts directory would be pruned, but in the servicemenus one it would sit
    /// there for ever.
    #[test]
    fn writing_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_if_changed(&dir.path().join("artifact"), "one", 0o755).expect("write");
        assert!(!dir.path().join(TMP_NAME).exists());
    }

    #[test]
    fn removing_something_absent_is_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        remove_if_present(&dir.path().join("nope")).expect("absent is fine");
    }

    #[test]
    fn only_a_file_carrying_the_marker_is_ours() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ours = dir.path().join("ours");
        std::fs::write(&ours, format!("#!/bin/sh\n# {MARKER}\n")).expect("write");
        let theirs = dir.path().join("theirs");
        std::fs::write(&theirs, "#!/bin/sh\necho hello\n").expect("write");

        assert!(is_ours(&ours));
        assert!(!is_ours(&theirs));
        assert!(!is_ours(&dir.path().join("absent")));
    }

    /// A big file a stranger left in the directory must not be read whole just to
    /// answer "is this ours" — and must not be claimed either.
    #[test]
    fn a_large_foreign_file_is_not_ours() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big");
        let mut content = "x".repeat(64 * 1024);
        content.push_str(MARKER);
        std::fs::write(&path, content).expect("write");
        assert!(
            !is_ours(&path),
            "the marker must be looked for in the head, not the whole file"
        );
    }

    /// The sweep's whole contract: our marked files go unless they are wanted,
    /// everything else stays and is counted — because that count is what decides
    /// whether a directory of ours may be removed.
    #[test]
    fn the_sweep_removes_our_files_and_counts_what_it_must_leave() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ours = |name: &str| {
            std::fs::write(dir.path().join(name), format!("#!/bin/sh\n# {MARKER}\n"))
                .expect("write")
        };
        ours("wanted");
        ours("an older name");
        std::fs::write(dir.path().join("theirs"), "#!/bin/sh\necho mine\n").expect("write");
        std::fs::create_dir(dir.path().join("their folder")).expect("mkdir");

        let swept = sweep(dir.path(), &HashSet::from(["wanted"])).expect("sweep");
        assert_eq!(listing(dir.path()), ["their folder", "theirs", "wanted"]);
        assert_eq!(
            swept.left, 3,
            "a directory and a foreign file both keep a directory alive"
        );

        // And with nothing wanted, only what is not ours is left.
        let swept = sweep(dir.path(), &HashSet::new()).expect("sweep");
        assert_eq!(listing(dir.path()), ["their folder", "theirs"]);
        assert_eq!(swept.left, 2);
    }

    /// A directory that was never written to is not an error: `apply(&[])` runs at
    /// every startup, usually with nothing to sweep.
    #[test]
    fn sweeping_a_directory_that_is_not_there_is_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let swept = sweep(&dir.path().join("never"), &HashSet::new()).expect("absent is fine");
        assert_eq!(swept.left, 0);
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_label_is_the_name_without_its_surrounding_whitespace() {
        assert_eq!(label_of("  PC A \t"), "PC A");
        assert_eq!(label_of("PC A"), "PC A");
    }
}
