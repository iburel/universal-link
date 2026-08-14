// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! "Send to": one shortcut per device in the user's `SendTo` folder.
//!
//! The flattest surface there is — the shell shows each file in that folder as a
//! menu entry, labelled with the file's own display name (a `.lnk`'s extension is
//! always hidden). Which decides two things:
//! - the entries carry the product's name themselves, `PC A (1Device)`,
//!   because there is no submenu to put them in (decision 3 of the plan);
//! - the label IS a file name, so it is sanitized rather than escaped — the same
//!   answer the Nautilus scripts needed, for the same reason, with Windows' own
//!   rules (see [`file_label`](super::file_label)).
//!
//! # Why a `.lnk` and not a script
//!
//! Because "Send to" is a **drop**: the shell drops the selection onto the entry,
//! and a shortcut to an executable hands the dropped paths to it as arguments,
//! appended after the ones the shortcut already carries. That is what lets one
//! entry mean one device — the `--send <id> --` is in the shortcut, the paths come
//! from the drop. It also means a whole selection arrives in ONE process here,
//! unlike the classic menu; the command line still has a length limit, which is one
//! more reason the manager coalesces (see `clicks`).
//!
//! A `.cmd` in the folder would work too and needs no COM, but its extension shows
//! in the menu and it flashes a console window at every click.
//!
//! # The marker lives in the description
//!
//! Pruning means enumerating a folder full of the user's own shortcuts, so
//! [`MARKER`] has to be somewhere we can read back. The description is that place:
//! the shell does not show it in a menu (a menu item has no tooltip), it survives an
//! install moving to another folder, and a file we cannot read as a shortcut at all
//! is simply not ours.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IPersistFile, STGM_READ,
};
use windows::Win32::UI::Shell::{
    FOLDERID_SendTo, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath, SLGP_RAWPATH, ShellLink,
};
use windows::core::{HSTRING, Interface};

use super::{MARKER, SUFFIX, entries, file_label, fit_utf16, is_file, quote, remove_if_present};
use crate::surface::{HelperCommand, MenuSurface, Target};

/// Longest a shortcut's name may be before the suffix and the extension, in UTF-16
/// units. A file name is capped at 255 of them; this leaves room for
/// ` (1Device).lnk` and for a disambiguating id.
const BASE_BUDGET: usize = 180;
/// And how much of a device id may appear in a name. Ids are `d_` plus 16 hex
/// digits; this is only a bound on what a hostile directory could make us write.
const ID_BUDGET: usize = 40;
/// Buffer for the strings read back out of a shortcut. `INFOTIPSIZE` for the
/// description, `MAX_PATH`-and-then-some for the rest — a value longer than this
/// cannot be one of ours anyway, and the comparison then just says "rewrite".
const READ_BUFFER: usize = 1024;

/// The user's `SendTo` folder. Asked for rather than assumed: it is a known folder
/// and can be redirected, and `%APPDATA%\Microsoft\Windows\SendTo` is only its
/// default.
///
/// Public for the live suite, which writes into the real one.
pub fn folder() -> io::Result<PathBuf> {
    let _com = Apartment::enter();
    // SAFETY: a known-folder id and no token (the current user); the returned
    // buffer is freed with CoTaskMemFree, as the API requires.
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_SendTo, KF_FLAG_DEFAULT, None)
            .map_err(|e| io::Error::other(format!("SHGetKnownFolderPath(SendTo): {e}")))?;
        let value = path.to_string();
        CoTaskMemFree(Some(path.0 as *const std::ffi::c_void));
        Ok(PathBuf::from(value.map_err(|e| {
            io::Error::other(format!("the SendTo path is not valid Unicode: {e}"))
        })?))
    }
}

pub struct SendTo {
    dir: PathBuf,
    helper: HelperCommand,
}

impl SendTo {
    pub fn new(dir: &Path, helper: HelperCommand) -> SendTo {
        SendTo {
            dir: dir.to_path_buf(),
            helper,
        }
    }

    /// Removes every shortcut of ours that is no longer wanted.
    ///
    /// The folder belongs to the user — Windows itself puts entries there, and so
    /// does anyone who ever dragged a shortcut into it — so a file is deleted only
    /// when it is a shortcut whose description carries our marker.
    fn prune(&self, wanted: &HashSet<String>) -> io::Result<()> {
        for path in entries(&self.dir)? {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if wanted.contains(&name) || !is_file(&path) {
                continue;
            }
            if read_shortcut(&path).is_some_and(|link| link.description.contains(MARKER)) {
                remove_if_present(&path)?;
            }
        }
        Ok(())
    }
}

impl MenuSurface for SendTo {
    fn name(&self) -> &'static str {
        "windows-send-to"
    }

    fn apply(&mut self, targets: &[Target]) -> io::Result<()> {
        let wanted = plan(&self.helper, targets);
        for (name, shortcut) in &wanted {
            let path = self.dir.join(name);
            // Skipped when nothing would change: this is a folder the user can
            // have open, and the applier re-applies the current list whenever it
            // cannot prove nothing moved.
            if read_shortcut(&path).as_ref() == Some(shortcut) {
                continue;
            }
            std::fs::create_dir_all(&self.dir)?;
            write_shortcut(&path, shortcut)?;
        }
        self.prune(&wanted.into_iter().map(|(name, _)| name).collect())
    }
}

/// What one shortcut has to say.
#[derive(Debug, PartialEq, Eq)]
struct Shortcut {
    /// Our own executable. Absolute: an entry outlives our process.
    target: String,
    /// The courier's fixed arguments. The dropped paths land after them.
    arguments: String,
    /// Shown nowhere in the menu, and where [`MARKER`] lives.
    description: String,
}

/// The shortcuts a target list means, by file name.
fn plan(helper: &HelperCommand, targets: &[Target]) -> Vec<(String, Shortcut)> {
    let names = file_names(targets);
    names
        .into_iter()
        .zip(targets)
        .map(|(name, target)| {
            let arguments = helper
                .args_for(target)
                .iter()
                .map(|arg| quote(arg))
                .collect::<Vec<_>>()
                .join(" ");
            let shortcut = Shortcut {
                target: helper.program.to_string_lossy().into_owned(),
                arguments,
                description: format!(
                    "Send the selection to {} with 1Device. {MARKER} — do not edit.",
                    file_label(&target.name)
                ),
            };
            (name, shortcut)
        })
        .collect()
}

/// One distinct file name per target, in the order they were given.
///
/// The ladder is the same idea as the Nautilus scripts': prefer the plain label,
/// and when two devices share one, disambiguate BOTH — an entry that changes name
/// depending on which sibling is online would be worse than a suffix. The
/// comparison is case-insensitive because the file system is: `PC` and `pc` are two
/// labels but one file, and each render would otherwise overwrite the other's.
fn file_names(targets: &[Target]) -> Vec<String> {
    let bases: Vec<String> = targets.iter().map(base_of).collect();
    let mut used: HashSet<String> = HashSet::new();
    let mut names = Vec::with_capacity(targets.len());
    for (index, (base, target)) in bases.iter().zip(targets).enumerate() {
        let shared = bases
            .iter()
            .filter(|other| other.to_lowercase() == base.to_lowercase())
            .count()
            > 1;
        let id = fit_utf16(&target.device_id, ID_BUDGET);
        let tail: String = id
            .chars()
            .skip(id.chars().count().saturating_sub(4))
            .collect();
        let candidates = [
            (!shared).then(|| base.clone()),
            Some(format!("{base} ({tail})")),
            Some(format!("{base} ({id})")),
            Some(id.clone()),
            // Always available, always unique: the index cannot repeat.
            Some(format!("{id} ({index})")),
        ];
        let name = candidates
            .into_iter()
            .flatten()
            .map(|stem| file_name(&stem))
            .find(|name| !used.contains(&name.to_lowercase()))
            .expect("the last candidate is unique by construction");
        used.insert(name.to_lowercase());
        names.push(name);
    }
    names
}

/// The base label for a target: its name as a file name, or its id when the name
/// leaves nothing usable.
fn base_of(target: &Target) -> String {
    let base = file_label(&target.name);
    if base.is_empty() {
        return fit_utf16(&target.device_id, ID_BUDGET);
    }
    base
}

/// A stem, made into the file name the shell will show without the extension.
fn file_name(stem: &str) -> String {
    // A reserved device name (`CON`, `NUL`, `COM1`…) can never come out of this:
    // the suffix is always there, so the name is never one of them.
    format!("{}{SUFFIX}.lnk", fit_utf16(stem, BASE_BUDGET))
}

// ---------------------------------------------------------------------------
// The COM part: a shortcut is a `ShellLink`.
// ---------------------------------------------------------------------------

/// An initialized COM apartment for the duration of one call.
///
/// `apply` runs on whichever blocking thread the applier had free, so the apartment
/// cannot be entered once and kept. Re-entering is cheap and reference-counted; the
/// case that needs care is a thread that already has a DIFFERENT apartment model,
/// where we must NOT undo what somebody else set up.
struct Apartment {
    ours: bool,
}

impl Apartment {
    fn enter() -> Apartment {
        // SAFETY: no reserved argument, and the matching CoUninitialize is in
        // `drop` — unless the thread was already in another apartment, which is
        // the one case where the call did not take a reference.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        Apartment {
            ours: hr != RPC_E_CHANGED_MODE && hr.is_ok(),
        }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.ours {
            // SAFETY: balances exactly one successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
}

fn new_link() -> io::Result<IShellLinkW> {
    // SAFETY: an in-process class, no aggregation.
    unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| io::Error::other(format!("cannot create a shell link: {e}")))
}

fn write_shortcut(path: &Path, shortcut: &Shortcut) -> io::Result<()> {
    let _com = Apartment::enter();
    let link = new_link()?;
    // SAFETY: every argument is a NUL-terminated string owned by this scope; the
    // interfaces come from the object created just above.
    unsafe {
        let set = || -> windows::core::Result<()> {
            link.SetPath(&HSTRING::from(&shortcut.target))?;
            link.SetArguments(&HSTRING::from(&shortcut.arguments))?;
            link.SetDescription(&HSTRING::from(&shortcut.description))?;
            let file: IPersistFile = link.cast()?;
            // `true` = remember this as the object's own file name, which is what
            // a shortcut on disk is.
            file.Save(&HSTRING::from(path.as_os_str()), true)
        };
        set().map_err(|e| io::Error::other(format!("cannot write {}: {e}", path.display())))
    }
}

/// Reads a shortcut back, or `None` if the file is not one (or cannot be read).
///
/// Not being readable is the fail-closed answer twice over: an unreadable file is
/// never ours to delete, and a shortcut we cannot compare is simply rewritten.
fn read_shortcut(path: &Path) -> Option<Shortcut> {
    let _com = Apartment::enter();
    let link = new_link().ok()?;
    // SAFETY: as above; each getter is given a buffer and its true length.
    unsafe {
        let file: IPersistFile = link.cast().ok()?;
        file.Load(&HSTRING::from(path.as_os_str()), STGM_READ)
            .ok()?;
        let mut target = [0u16; READ_BUFFER];
        // No WIN32_FIND_DATAW wanted: the shortcut's own bytes are the question,
        // not the state of the file it names — and RAWPATH is what asks for those
        // bytes rather than a resolved, expanded answer.
        link.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
            .ok()?;
        let mut arguments = [0u16; READ_BUFFER];
        link.GetArguments(&mut arguments).ok()?;
        let mut description = [0u16; READ_BUFFER];
        link.GetDescription(&mut description).ok()?;
        Some(Shortcut {
            target: from_wide(&target),
            arguments: from_wide(&arguments),
            description: from_wide(&description),
        })
    }
}

/// A NUL-terminated UTF-16 buffer as a string.
fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os::windows::tests::{helper, parse_command_line, target};

    /// A shortcut Windows really wrote, read back through the same COM interface
    /// Explorer uses — including the argv a drop will extend.
    #[test]
    fn a_shortcut_carries_the_courier_command_for_its_device() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = SendTo::new(dir.path(), helper());
        surface.apply(&[target("d_aaa", "PC-A")]).expect("apply");

        let path = dir.path().join("PC-A (1Device).lnk");
        let link = read_shortcut(&path).expect("the shortcut must be readable");
        assert_eq!(link.target, r"C:\Program Files\UL\1device-menu.exe");
        assert!(
            link.description.contains(MARKER),
            "without the marker it could never be pruned: {}",
            link.description
        );
        // The shell appends the dropped paths after these, so this is the argv a
        // click produces, minus the selection.
        assert_eq!(
            parse_command_line(&format!(r#""{}" {}"#, link.target, link.arguments)),
            [
                r"C:\Program Files\UL\1device-menu.exe",
                "--send",
                "d_aaa",
                "--"
            ]
        );
    }

    /// The arguments of a shortcut are a command line too, and a live test injects a
    /// channel path into them — a pipe name that can hold a space. Production passes
    /// none, so this is the only case where the quoting here is load-bearing, and the
    /// real parser is what says whether it worked.
    #[test]
    fn an_injected_channel_survives_the_shortcut_arguments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let helper = HelperCommand {
            program: PathBuf::from(r"C:\ul\menu.exe"),
            extra_args: vec!["--channel".into(), r"\\.\pipe\ul menu live".into()],
        };
        let mut surface = SendTo::new(dir.path(), helper);
        surface.apply(&[target("d_aaa", "PC-A")]).expect("apply");

        let link = read_shortcut(&dir.path().join("PC-A (1Device).lnk")).expect("readable");
        assert_eq!(
            parse_command_line(&format!(r#""{}" {}"#, link.target, link.arguments)),
            [
                r"C:\ul\menu.exe",
                "--channel",
                r"\\.\pipe\ul menu live",
                "--send",
                "d_aaa",
                "--"
            ]
        );
    }

    /// A device with no usable name still needs an entry, and a distinct one: the
    /// fallback is its id.
    #[test]
    fn a_device_whose_name_leaves_nothing_falls_back_to_its_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = SendTo::new(dir.path(), helper());
        surface
            .apply(&[target("d_1111", "   "), target("d_2222", "...")])
            .expect("apply");

        assert!(dir.path().join("d_1111 (1Device).lnk").exists());
        assert!(dir.path().join("d_2222 (1Device).lnk").exists());
    }

    /// The folder can be open in Explorer, and the applier re-applies the list it
    /// already rendered whenever it cannot prove nothing changed.
    #[test]
    fn an_identical_apply_leaves_the_shortcut_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = SendTo::new(dir.path(), helper());
        let targets = [target("d_aaa", "PC-A")];
        surface.apply(&targets).expect("apply");

        let path = dir.path().join("PC-A (1Device).lnk");
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("set mtime");

        surface.apply(&targets).expect("reapply");
        assert_eq!(
            std::fs::metadata(&path).expect("meta").modified().ok(),
            Some(old),
            "an unchanged shortcut was rewritten"
        );
    }

    /// No manager, no entry — and nothing of anyone else's touched on the way out.
    #[test]
    fn an_empty_list_removes_our_shortcuts_and_only_ours() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = SendTo::new(dir.path(), helper());
        surface
            .apply(&[target("d_aaa", "PC-A"), target("d_bbb", "PC-B")])
            .expect("apply");

        // What the user's own SendTo folder really holds.
        let theirs = dir.path().join("Bluetooth (1Device).lnk");
        write_shortcut(
            &theirs,
            &Shortcut {
                target: r"C:\Windows\System32\fsquirt.exe".into(),
                arguments: String::new(),
                description: "someone else's".into(),
            },
        )
        .expect("write theirs");
        let plain = dir.path().join("Documents.mydocs");
        std::fs::write(&plain, b"").expect("write");

        surface.apply(&[]).expect("clear");

        assert!(!dir.path().join("PC-A (1Device).lnk").exists());
        assert!(!dir.path().join("PC-B (1Device).lnk").exists());
        assert!(
            theirs.exists(),
            "a shortcut without our marker was deleted, even with our own naming"
        );
        assert!(plain.exists(), "a file that is not a shortcut was deleted");

        // Applying nothing twice is what every startup does.
        surface.apply(&[]).expect("clear again");
    }

    /// An entry a previous run left behind must go, or a click on it reaches no
    /// manager and fails silently.
    #[test]
    fn a_stale_entry_of_ours_is_swept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale = dir.path().join("Old-PC (1Device).lnk");
        write_shortcut(
            &stale,
            &Shortcut {
                target: r"C:\Program Files\UL\1device-menu.exe".into(),
                arguments: "--send d_gone --".into(),
                description: format!("stale. {MARKER} — do not edit."),
            },
        )
        .expect("write stale");

        let mut surface = SendTo::new(dir.path(), helper());
        surface.apply(&[]).expect("clear");
        assert!(!stale.exists());
    }

    /// A label is a file name here, so two devices called the same thing would be
    /// ONE entry. Both are disambiguated, not just the second: an entry must not
    /// change name depending on which sibling happens to be online.
    #[test]
    fn devices_that_share_a_name_get_one_entry_each() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = SendTo::new(dir.path(), helper());
        surface
            .apply(&[target("d_1111", "PC"), target("d_2222", "PC")])
            .expect("apply");

        assert!(dir.path().join("PC (1111) (1Device).lnk").exists());
        assert!(dir.path().join("PC (2222) (1Device).lnk").exists());
        assert!(!dir.path().join("PC (1Device).lnk").exists());
    }

    /// And the case that is Windows' alone: the file system does not distinguish
    /// `PC` from `pc`, so two labels that differ only in case are still one file —
    /// and BOTH must be disambiguated, not just whichever came second. An entry
    /// named after its sibling's existence would change name depending on which of
    /// the two is online.
    #[test]
    fn names_that_differ_only_in_case_are_still_two_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = SendTo::new(dir.path(), helper());
        surface
            .apply(&[target("d_1111", "PC"), target("d_2222", "pc")])
            .expect("apply");

        let mut files: Vec<String> = entries(dir.path())
            .expect("entries")
            .iter()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        files.sort();
        assert_eq!(
            files,
            ["PC (1111) (1Device).lnk", "pc (2222) (1Device).lnk"],
            "the two entries must be symmetrical, and there must be two"
        );
    }

    /// The escaping obligation on this surface: the name is a file name (it cannot
    /// leave the folder, cannot hold a character Win32 refuses, cannot end in a dot
    /// Win32 would drop) and the arguments are argv (the name never reaches them).
    #[test]
    fn a_hostile_name_cannot_leave_the_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = SendTo::new(dir.path(), helper());
        surface
            .apply(&[target("d_aaa", r#"..\..\Startup\evil" & del *.* "#)])
            .expect("apply");

        let files = entries(dir.path()).expect("entries");
        assert_eq!(files.len(), 1, "exactly one file, and in this folder");
        let name = files[0]
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert_eq!(name, "..-..-Startup-evil- & del -.- (1Device).lnk");
        let link = read_shortcut(&files[0]).expect("readable");
        assert_eq!(
            parse_command_line(&format!(r#""{}" {}"#, link.target, link.arguments)),
            [
                r"C:\Program Files\UL\1device-menu.exe",
                "--send",
                "d_aaa",
                "--"
            ]
        );
    }

    /// A name longer than a file name can be must still produce one — and stay
    /// distinct from its neighbour.
    #[test]
    fn a_very_long_name_still_makes_a_writable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = SendTo::new(dir.path(), helper());
        let long = "é".repeat(400);
        surface
            .apply(&[target("d_1111", &long), target("d_2222", &long)])
            .expect("apply");

        let files = entries(dir.path()).expect("entries");
        assert_eq!(files.len(), 2);
        for path in files {
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            assert!(
                name.encode_utf16().count() <= 255,
                "a name Windows cannot store: {} units",
                name.encode_utf16().count()
            );
            assert!(read_shortcut(&path).is_some(), "unreadable: {name}");
        }
    }

    /// The real folder, on the real machine: it has to resolve, and it has to be
    /// the user's.
    #[test]
    fn the_send_to_folder_resolves() {
        let dir = folder().expect("the SendTo folder must resolve");
        assert!(dir.is_absolute(), "not absolute: {}", dir.display());
        assert!(
            dir.ends_with("SendTo"),
            "unexpected SendTo folder: {}",
            dir.display()
        );
    }
}
