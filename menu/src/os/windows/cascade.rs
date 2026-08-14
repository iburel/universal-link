// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The classic shortcut menu's cascading entry: `1Device ▸ PC A`.
//!
//! Two registrations of the same list, one per class Explorer asks about:
//! `*` for a selection of files, `Directory` for a folder. They are separate
//! surfaces because they are separate keys — a failure on one must not cost the
//! other its entries.
//!
//! # The shape of one cascade
//!
//! ```text
//! HKCU\Software\Classes\*\shell\1Device
//!     MUIVerb                 = 1Device
//!     ExtendedSubCommandsKey  = *\shell\1Device        (relative to HKCR)
//!     MultiSelectModel        = Player
//!     1DeviceGenerated  = 1device-menu:generated
//!   \shell\000-d_3a4424d810ba6c27
//!     MUIVerb                 = Anges-MacBook-Pro.local
//!     MultiSelectModel        = Player
//!     \command
//!         (default)           = "C:\…\1device-menu.exe" --send d_3a44… -- "%1"
//! ```
//!
//! Each key is there for a reason, and the reason is what shipping software does on
//! a real machine (see the module header of `os::windows`):
//! - `MUIVerb` is the displayed label. Not the key's default value: every cascade
//!   in `HKLM\Software\Classes` uses `MUIVerb`, and it is the one the shell reads
//!   for a submenu.
//! - `ExtendedSubCommandsKey` is what turns the verb into a submenu instead of a
//!   command. It names a key **relative to `HKEY_CLASSES_ROOT`** whose `shell`
//!   subkey holds the children — here, itself.
//! - `MultiSelectModel=Player` asks for ONE invocation carrying the whole
//!   selection, rather than one process per selected file. It is an optimization,
//!   not a guarantee: the manager coalesces either way (see `clicks`).
//! - the verb keys are named `NNN-<device id>`. The number is what puts them in
//!   the order we sorted the targets in — the registry hands subkeys back sorted by
//!   name — and starting with a digit keeps a device from ever taking the name of a
//!   canonical verb (`open`, `runas`, `print`), whose meaning is not ours.
//! - `"%1"` comes LAST in the command line, because that is where the shell appends
//!   the rest of a multi-selection.

use std::io;

use super::registry::Key;
use super::{MARKER, VERB, command_prefix, menu_label};
use crate::surface::{HelperCommand, MenuSurface, Target};

/// Value that marks the cascade as ours. Nothing without it is ever deleted.
const MARKER_VALUE: &str = "1DeviceGenerated";
/// Label of the submenu itself.
const SUBMENU: &str = "1Device";
/// One invocation for a whole selection, rather than one process per file.
const MULTI_SELECT: &str = "Player";

pub struct Cascade {
    name: &'static str,
    /// `Software\Classes\*\shell` — where the verb key lives, and what has to be
    /// opened to delete it.
    parent: String,
    /// `Software\Classes\*\shell\1Device`
    key: String,
    /// What `ExtendedSubCommandsKey` must say. Relative to `HKEY_CLASSES_ROOT` by
    /// definition, so it never carries the root the keys are written under — which
    /// is also why a test can move that root without making this value a lie.
    extended: String,
    helper: HelperCommand,
}

impl Cascade {
    /// The cascade shown on a selection of files.
    pub fn files(classes: &str, helper: HelperCommand) -> Cascade {
        Cascade::new(classes, "*", "windows-cascade-files", helper)
    }

    /// And the one shown on a folder. `*` does not cover directories.
    pub fn folders(classes: &str, helper: HelperCommand) -> Cascade {
        Cascade::new(classes, "Directory", "windows-cascade-folders", helper)
    }

    fn new(classes: &str, class: &str, name: &'static str, helper: HelperCommand) -> Cascade {
        Cascade {
            name,
            parent: format!(r"{classes}\{class}\shell"),
            key: format!(r"{classes}\{class}\shell\{VERB}"),
            extended: format!(r"{class}\shell\{VERB}"),
            helper,
        }
    }

    /// The key holding the child verbs.
    fn children(&self) -> String {
        format!(r"{}\shell", self.key)
    }

    /// Whether the verb key is absent, or present and written by us.
    ///
    /// The rule of both Windows surfaces: enumerate what the user owns, delete only
    /// what carries our marker. A verb of this name that we did not write belongs
    /// to something else, and its subkeys are not ours to remove.
    fn ours(&self) -> io::Result<bool> {
        match Key::open(&self.key)? {
            None => Ok(true),
            Some(key) => Ok(key.string(MARKER_VALUE).as_deref() == Some(MARKER)),
        }
    }

    fn remove(&self) -> io::Result<()> {
        if !self.ours()? {
            // Nothing of ours to remove. Not an error: there is no work left.
            return Ok(());
        }
        match Key::open(&self.parent)? {
            Some(parent) => parent.delete_subtree(VERB),
            // No `shell` key at all — nothing was ever written.
            None => Ok(()),
        }
    }

    /// Deletes every OTHER verb under the same `shell` key that carries our marker.
    ///
    /// The child sweep's rule, one level up. The shell reads every subkey of `shell`,
    /// so a cascade of ours under a name this version no longer writes is not
    /// dormant: it is a live submenu, frozen on the device list of the day it was
    /// written, whose every click the manager refuses. Sweeping by marker instead of
    /// by expected name is what makes renaming [`VERB`] self-healing on upgrade, and
    /// it needs no list of retired names — the kind of list nothing cross-checks.
    ///
    /// A verb without the marker is left alone whatever it is called, which is the
    /// same rule as [`Cascade::ours`]: these are the user's keys, not ours. Rare
    /// company, measured: `HKCU\Software\Classes\*\shell` is empty on a real install
    /// (`tests/windows.rs`) — a machine's verbs live under `HKLM`, which this
    /// component never writes — but a program that registers a per-user verb exists,
    /// and the sweep must be safe beside it.
    fn sweep_older_verbs(&self, keep: Option<&str>) -> io::Result<()> {
        let Some(parent) = Key::open(&self.parent)? else {
            // No `shell` key at all — nothing was ever written under this class.
            return Ok(());
        };
        for name in parent.subkeys()? {
            if keep == Some(name.as_str()) {
                continue;
            }
            // A sibling we cannot even open counts as not ours, rather than failing
            // the whole render: these keys are not ours, and one of them being
            // unreadable must cost a stale key at worst, never the entries
            // themselves. Same call as `is_ours` on Linux, for the same reason.
            let marked = Key::open(&format!(r"{}\{name}", self.parent))
                .ok()
                .flatten()
                .is_some_and(|key| key.string(MARKER_VALUE).as_deref() == Some(MARKER));
            if marked {
                parent.delete_subtree(&name)?;
            }
        }
        Ok(())
    }
}

impl MenuSurface for Cascade {
    fn name(&self) -> &'static str {
        self.name
    }

    fn apply(&mut self, targets: &[Target]) -> io::Result<()> {
        if targets.is_empty() {
            self.remove()?;
            return self.sweep_older_verbs(None);
        }
        if !self.ours()? {
            return Err(io::Error::other(format!(
                "HKCU\\{} exists and is not ours: left untouched",
                self.key
            )));
        }

        // The children first, then the values that make the parent a cascade: a
        // right click in between then finds an entry that is not yet a submenu
        // rather than a submenu that is empty. (A window of a few microseconds
        // either way — the registry has no atomic rename to do better.)
        let mut wanted = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let verb = verb_name(index, target);
            let child = Key::create(&format!(r"{}\{verb}", self.children()))?;
            child.set_string("MUIVerb", &menu_label(&target.name))?;
            child.set_string("MultiSelectModel", MULTI_SELECT)?;
            let command = Key::create(&format!(r"{}\{verb}\command", self.children()))?;
            command.set_string("", &command_line(&self.helper, target))?;
            wanted.push(verb);
        }

        let key = Key::create(&self.key)?;
        key.set_string(MARKER_VALUE, MARKER)?;
        key.set_string("MUIVerb", SUBMENU)?;
        key.set_string("ExtendedSubCommandsKey", &self.extended)?;
        key.set_string("MultiSelectModel", MULTI_SELECT)?;

        // Absolute, not incremental: whatever a previous list left behind goes.
        // Everything here is inside our own marked key, which is what makes
        // deleting it safe.
        let children = Key::create(&self.children())?;
        for name in children.subkeys()? {
            if !wanted.contains(&name) {
                children.delete_subtree(&name)?;
            }
        }

        // And one level up, where only a previous VERSION of us can have left
        // something: our own verb is kept, anything else of ours goes.
        self.sweep_older_verbs(Some(VERB))
    }
}

/// The command line a click runs. `"%1"` is the shell's placeholder for the
/// selection, and it is last on purpose — see the module header.
///
/// Public because the live suite hands this exact string to the real shell and has
/// it invoked (`tests/windows.rs`): the shell's own parse, substitution and launch
/// are the only things no local test can stand in for.
pub fn command_line(helper: &HelperCommand, target: &Target) -> String {
    format!("{} \"%1\"", command_prefix(helper, target))
}

/// The verb key's name: the target's rank, then its device id.
///
/// The rank is what orders the submenu (subkeys come back sorted by name) and it is
/// also what keeps two devices apart no matter what their ids look like.
fn verb_name(index: usize, target: &Target) -> String {
    format!("{index:03}-{}", key_name_of(&target.device_id))
}

/// A device id, made safe to use as a registry key name.
///
/// The id is minted by the server (`d_<hex>`) and needs nothing — but it reaches us
/// as a JSON string, and a key name has no escape: a single backslash in it would
/// silently create a NESTED key, so what we then look for while pruning would never
/// be what we wrote, and the entry would be deleted and recreated on every render.
fn key_name_of(device_id: &str) -> String {
    device_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os::windows::tests::{TestRoot, helper, target};

    /// Reads a key's value back from the registry, failing the test if it is
    /// absent — these assertions are about what Explorer will find.
    fn value(path: &str, name: &str) -> String {
        Key::open(path)
            .expect("open")
            .unwrap_or_else(|| panic!("HKCU\\{path} does not exist"))
            .string(name)
            .unwrap_or_else(|| panic!("HKCU\\{path} has no {name:?} value"))
    }

    fn absent(path: &str) -> bool {
        Key::open(path).expect("open").is_none()
    }

    /// The whole shape, key by key, because every one of them is load-bearing: a
    /// missing `ExtendedSubCommandsKey` is a menu entry that does nothing, and a
    /// missing `MUIVerb` is one with no name.
    #[test]
    fn a_cascade_is_a_submenu_with_one_command_per_target() {
        let root = TestRoot::new("shape");
        let mut cascade = Cascade::files(root.classes(), helper());
        let targets = [target("d_aaa", "PC-A"), target("d_bbb", "PC-B")];
        cascade.apply(&targets).expect("apply");

        let key = format!(r"{}\*\shell\1Device", root.classes());
        assert_eq!(value(&key, "MUIVerb"), "1Device");
        assert_eq!(
            value(&key, "ExtendedSubCommandsKey"),
            r"*\shell\1Device",
            "without this the entry is a command, not a submenu"
        );
        assert_eq!(value(&key, "MultiSelectModel"), "Player");
        assert_eq!(value(&key, "1DeviceGenerated"), MARKER);

        // One child per target, in the order the targets came in.
        let children = Key::open(&format!(r"{key}\shell"))
            .expect("open")
            .expect("the children key");
        let mut names = children.subkeys().expect("subkeys");
        names.sort();
        assert_eq!(names, ["000-d_aaa", "001-d_bbb"]);

        assert_eq!(value(&format!(r"{key}\shell\000-d_aaa"), "MUIVerb"), "PC-A");
        assert_eq!(value(&format!(r"{key}\shell\001-d_bbb"), "MUIVerb"), "PC-B");
        assert_eq!(
            value(&format!(r"{key}\shell\001-d_bbb\command"), ""),
            r#""C:\Program Files\UL\1device-menu.exe" --send d_bbb -- "%1""#,
            "the command must name the clicked device and end with the selection"
        );
    }

    /// The folders cascade is the same list under another class: `*` does not
    /// cover directories, so without it right-clicking a folder offers nothing.
    #[test]
    fn folders_are_registered_under_their_own_class() {
        let root = TestRoot::new("folders");
        let mut cascade = Cascade::folders(root.classes(), helper());
        cascade.apply(&[target("d_aaa", "PC-A")]).expect("apply");

        let key = format!(r"{}\Directory\shell\1Device", root.classes());
        assert_eq!(
            value(&key, "ExtendedSubCommandsKey"),
            r"Directory\shell\1Device"
        );
        assert!(absent(&format!(r"{}\*\shell\1Device", root.classes())));
    }

    /// No manager, no entry: the fail-closed rule. `apply(&[])` is what runs at
    /// startup and at shutdown, and it must leave nothing behind.
    #[test]
    fn an_empty_list_removes_the_cascade_entirely() {
        let root = TestRoot::new("empty");
        let mut cascade = Cascade::files(root.classes(), helper());
        cascade.apply(&[target("d_aaa", "PC-A")]).expect("apply");
        cascade.apply(&[]).expect("clear");

        assert!(absent(&format!(r"{}\*\shell\1Device", root.classes())));
        // Twice is fine: an empty list is applied at every startup.
        cascade.apply(&[]).expect("clear again");
    }

    /// A device that went offline must lose its entry, which is the only thing an
    /// absolute surface can do: the key is enumerated and pruned.
    #[test]
    fn a_target_that_is_gone_loses_its_entry() {
        let root = TestRoot::new("prune");
        let mut cascade = Cascade::files(root.classes(), helper());
        cascade
            .apply(&[target("d_aaa", "PC-A"), target("d_bbb", "PC-B")])
            .expect("apply");
        cascade.apply(&[target("d_bbb", "PC-B")]).expect("reapply");

        let key = format!(r"{}\*\shell\1Device", root.classes());
        let children = Key::open(&format!(r"{key}\shell"))
            .expect("open")
            .expect("children");
        assert_eq!(
            children.subkeys().expect("subkeys"),
            ["000-d_bbb"],
            "the departed device's verb must be gone, and the survivor renumbered"
        );
    }

    /// Never delete what we did not write. The marker is the whole test: a verb of
    /// this name without it belongs to someone else, and both installing over it
    /// and removing it would destroy their work.
    #[test]
    fn a_key_of_the_same_name_that_is_not_ours_is_never_touched() {
        let root = TestRoot::new("foreign");
        let key = format!(r"{}\*\shell\1Device", root.classes());
        let foreign = Key::create(&format!(r"{key}\shell\theirs")).expect("create");
        foreign
            .set_string("MUIVerb", "Someone else's")
            .expect("set");

        let mut cascade = Cascade::files(root.classes(), helper());
        assert!(
            cascade.apply(&[target("d_aaa", "PC-A")]).is_err(),
            "installing over a foreign key must be refused, not silently done"
        );
        cascade.apply(&[]).expect("removing must not fail either");

        assert_eq!(
            value(&format!(r"{key}\shell\theirs"), "MUIVerb"),
            "Someone else's",
            "a foreign entry was destroyed"
        );
    }

    /// The stale-artifact rule: the shell reads every subkey of `shell`, so a cascade
    /// we wrote under a name this version no longer uses is a live submenu listing
    /// whatever devices were online when it was written. It goes on the next render —
    /// and the verb beside it that carries no marker stays, which is the only reason
    /// enumerating the user's `shell` key is safe.
    ///
    /// Both renders, because the empty one is not the lesser case: `apply(&[])` is what
    /// runs at startup, and on a machine with nothing online it is the only render of
    /// the whole session.
    #[test]
    fn a_cascade_of_ours_under_an_older_name_is_swept() {
        let root = TestRoot::new("stale");
        let shell = format!(r"{}\*\shell", root.classes());
        let older = format!(r"{shell}\1DeviceSend");
        let plant = || {
            let stale = Key::create(&older).expect("create");
            stale.set_string(MARKER_VALUE, MARKER).expect("set");
            stale.set_string("MUIVerb", "Ghost").expect("set");
            // A child, to prove the whole subtree goes and not just the key.
            Key::create(&format!(r"{older}\shell\000-d_old")).expect("create");
        };
        let theirs = Key::create(&format!(r"{shell}\SomeoneElseSend")).expect("create");
        theirs.set_string("MUIVerb", "Theirs").expect("set");
        let mut cascade = Cascade::files(root.classes(), helper());

        plant();
        cascade.apply(&[]).expect("startup render");
        assert!(absent(&older), "the older name survived the startup render");

        plant();
        cascade.apply(&[target("d_aaa", "PC-A")]).expect("apply");
        assert!(
            absent(&older),
            "the older name survived a render with a list"
        );
        assert!(
            !absent(&format!(r"{shell}\1Device")),
            "our own verb must not be swept with it"
        );

        cascade.apply(&[]).expect("clear");
        assert!(absent(&format!(r"{shell}\1Device")));
        assert_eq!(
            value(&format!(r"{shell}\SomeoneElseSend"), "MUIVerb"),
            "Theirs",
            "a verb without our marker is never ours to delete"
        );
    }

    /// The escaping obligation, on the surface where the answer is "nothing to
    /// escape, and two things to defuse". A registry value is a counted string, so
    /// a device name cannot break out of one — but the shell reads `&` as a
    /// mnemonic and a leading `@` as a resource reference, and the command line is
    /// parsed by `CommandLineToArgvW`, which is asked here to prove it reads back
    /// exactly the arguments we meant.
    #[test]
    fn a_hostile_device_name_stays_inside_its_value() {
        let root = TestRoot::new("hostile");
        let mut cascade = Cascade::files(root.classes(), helper());
        let hostile = "@PC\" & del /q C:\\* & echo \nowned";
        cascade
            .apply(&[target("d_aaa", hostile)])
            .expect("a name is data, and must never fail a write");

        let key = format!(r"{}\*\shell\1Device", root.classes());
        let label = value(&format!(r"{key}\shell\000-d_aaa"), "MUIVerb");
        assert_eq!(label, "PC\" && del /q C:\\* && echo  owned");

        // And the command line is still exactly our four arguments.
        let command = value(&format!(r"{key}\shell\000-d_aaa\command"), "");
        assert_eq!(
            crate::os::windows::tests::parse_command_line(&command),
            [
                r"C:\Program Files\UL\1device-menu.exe",
                "--send",
                "d_aaa",
                "--",
                "%1",
            ],
            "the label must not have leaked into the command line"
        );
    }

    /// A key name has no escape: a backslash in a device id would create a nested
    /// key, and everything about the entry — writing it, finding it again, pruning
    /// it — would be about a name we never wrote.
    #[test]
    fn a_device_id_that_is_not_a_key_name_is_made_into_one() {
        assert_eq!(key_name_of("d_3a4424d810ba6c27"), "d_3a4424d810ba6c27");
        assert_eq!(key_name_of(r"d_1\command"), "d_1_command");
        assert_eq!(key_name_of("d 1/2*3"), "d_1_2_3");

        let root = TestRoot::new("keyname");
        let mut cascade = Cascade::files(root.classes(), helper());
        cascade.apply(&[target(r"d_1\Run", "PC-A")]).expect("apply");
        let children = Key::open(&format!(r"{}\*\shell\1Device\shell", root.classes()))
            .expect("open")
            .expect("children");
        assert_eq!(children.subkeys().expect("subkeys"), ["000-d_1_Run"]);
    }

    /// Idempotence, and the reason it is worth the read: the applier re-applies the
    /// list it has already rendered whenever it cannot prove nothing changed.
    #[test]
    fn re_applying_the_same_list_writes_nothing() {
        let root = TestRoot::new("idempotent");
        let mut cascade = Cascade::files(root.classes(), helper());
        let targets = [target("d_aaa", "PC-A")];
        cascade.apply(&targets).expect("apply");

        let key = Key::open(&format!(r"{}\*\shell\1Device", root.classes()))
            .expect("open")
            .expect("key");
        assert!(
            !key.set_string("MUIVerb", "1Device").expect("set"),
            "an identical value must not be rewritten"
        );
        assert!(
            key.set_string("MUIVerb", "Something else").expect("set"),
            "a different value must be"
        );
        cascade.apply(&targets).expect("reapply");
        assert!(
            !key.set_string("MUIVerb", "1Device").expect("set"),
            "the reapply must have put the label back"
        );
    }
}
