// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Nautilus's context menu, as scripts.
//!
//! One executable `/bin/sh` script per target in a directory of our own —
//! Nautilus turns a subdirectory of `scripts/` into a submenu
//! (`update_directory_in_scripts_menu`), which is decision 3's shape, and it also
//! keeps our entries out of the 30-per-directory ceiling the top level shares with
//! whatever else the user has put there.
//!
//! # A label is a file name here
//!
//! That is the whole difficulty of this surface. Nautilus shows each script under
//! its file's display name, so the device name has to become a valid, visible,
//! unique file name:
//! - a `/` cannot appear in one at all;
//! - a name starting with `.` is hidden, and Nautilus filters hidden files out of
//!   the menu, so such a device would have no entry;
//! - two devices called "PC" would otherwise be one file, and the second write
//!   would silently take the first one's place.
//!
//! Unlike Dolphin's `Name=`, this cannot be solved by escaping: there is nothing
//! to escape *into*. So the label is sanitized, and — where two of them collide —
//! both are disambiguated with a piece of the device id, never just the second.
//!
//! # The script itself
//!
//! Nautilus runs it with the browsed folder as the working directory and the
//! selected names as arguments (`get_file_names_as_parameter_array` returns paths
//! relative to that folder), which is exactly what the courier's cwd-joining is
//! for. Everything is single-quoted and the selection is passed as `"$@"`, so a
//! file named `$(rm -rf ~)` is an argument and nothing else.
//!
//! Only the device *id* reaches the script's body; the label lives in the file
//! name. A rename therefore moves a file and rewrites nothing.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use super::{MARKER, label_of, sweep, write_if_changed};
use crate::surface::{HelperCommand, MenuSurface, Target};

/// Our own submenu under Nautilus's "Scripts".
const DIR_NAME: &str = "UniversalLink";
/// Nautilus only offers a script it can launch, which means the executable bit.
const MODE: u32 = 0o755;
/// The longest file name any Linux filesystem in practice accepts, in BYTES.
const NAME_MAX: usize = 255;
/// How much of that a sanitized label may take, leaving room for a disambiguating
/// suffix (`" (d_" + 16 hex + ")"` today, see `server/src/conn.rs`).
const BASE_BUDGET: usize = 180;

/// The Nautilus scripts surface.
pub struct Scripts {
    dir: PathBuf,
    helper: HelperCommand,
}

impl Scripts {
    pub fn new(data_home: &Path, helper: HelperCommand) -> Scripts {
        Scripts {
            dir: data_home.join("nautilus").join("scripts").join(DIR_NAME),
            helper,
        }
    }

    /// The directory we own. For the tests, and for looking at a real desktop by
    /// hand.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Deletes every entry of ours that is not in `keep`, then the directory
    /// itself if nothing is left — and finally anything of ours one level up.
    ///
    /// This is what makes the surface absolute rather than incremental: a device
    /// that went offline, a device that was renamed, an entry left by a previous
    /// version. Anything WITHOUT our marker is left strictly alone — enumerating a
    /// directory and deleting from it is how someone else's file gets destroyed.
    fn prune(&self, keep: &HashSet<&str>) -> io::Result<()> {
        let swept = sweep(&self.dir, keep)?;
        if swept.left == 0 {
            // An empty directory shows no submenu, so this is cosmetic — but "no
            // manager, no trace" is easier to trust when it is literal.
            let _ = std::fs::remove_dir(&self.dir);
        }

        // `scripts/` itself, where a version that did not use a submenu would have
        // put its entries: Nautilus reads every file there too, so one of ours left
        // behind is a live menu item frozen on an old device list. Our own directory
        // is a directory, so the sweep counts it and leaves it be — as it does the
        // user's own scripts and their own subdirectories, which carry no marker and
        // are therefore never anyone's to delete but theirs.
        if let Some(scripts) = self.dir.parent() {
            sweep(scripts, &HashSet::new())?;
        }
        Ok(())
    }
}

impl MenuSurface for Scripts {
    fn name(&self) -> &'static str {
        "nautilus-scripts"
    }

    fn apply(&mut self, targets: &[Target]) -> io::Result<()> {
        let wanted = plan(&self.helper, targets);
        for (name, content) in &wanted {
            write_if_changed(&self.dir.join(name), content, MODE)?;
        }
        self.prune(&wanted.iter().map(|(name, _)| name.as_str()).collect())
    }
}

/// The file name and body of every entry, in `targets`' order.
fn plan(helper: &HelperCommand, targets: &[Target]) -> Vec<(String, String)> {
    file_names(targets)
        .into_iter()
        .zip(targets)
        .map(|(name, target)| (name, script(helper, target)))
        .collect()
}

/// One distinct, valid file name per target.
fn file_names(targets: &[Target]) -> Vec<String> {
    let bases: Vec<String> = targets.iter().map(base_name).collect();
    let mut used: HashSet<String> = HashSet::new();
    let mut names = Vec::with_capacity(targets.len());
    for (index, target) in targets.iter().enumerate() {
        let base = &bases[index];
        // BOTH devices that would share a name are disambiguated, not just the
        // second: "PC" sitting next to "PC (a1b2)" tells the user nothing about
        // which of the two is which.
        let shared = bases.iter().filter(|other| *other == base).count() > 1;
        let name = [
            (!shared).then(|| base.clone()),
            Some(format!("{base} ({})", tail(&target.device_id))),
            Some(format!("{base} ({})", target.device_id)),
            // A device id is unique account-wide and short enough to survive
            // truncation, so a free candidate always exists.
            Some(target.device_id.clone()),
            Some(format!("{} ({index})", target.device_id)),
        ]
        .into_iter()
        .flatten()
        .map(|candidate| fit(&candidate, NAME_MAX))
        .find(|candidate| !used.contains(candidate))
        .unwrap_or_else(|| format!("{index}"));
        used.insert(name.clone());
        names.push(name);
    }
    names
}

/// The device name, made into something that can be a file name AND be seen.
fn base_name(target: &Target) -> String {
    let mut out = String::with_capacity(target.name.len());
    for c in label_of(&target.name).chars() {
        match c {
            // Not escapable: a file name simply cannot hold a separator.
            '/' => out.push('-'),
            // Legal in a file name, hostile everywhere it is displayed.
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    // A leading dot hides the file, and a hidden file is filtered out of the menu
    // (`nautilus_file_should_show`) — a PC named ".maison" would have no entry at
    // all. It also disposes of "." and "..", which are not names.
    let cleaned = fit(out.trim().trim_start_matches('.').trim(), BASE_BUDGET);
    if cleaned.is_empty() {
        return target.device_id.clone();
    }
    cleaned
}

/// The tail of a device id: enough to tell two same-named devices apart without
/// putting a 18-character identifier in a menu.
fn tail(device_id: &str) -> &str {
    let start = device_id.len().saturating_sub(4);
    device_id.get(start..).unwrap_or(device_id)
}

/// Truncates to `max` BYTES on a character boundary — a file name is bounded in
/// bytes, not in characters, and a name is arbitrary Unicode.
fn fit(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].trim_end().to_string()
}

/// The body of one entry.
fn script(helper: &HelperCommand, target: &Target) -> String {
    let mut command = vec![quote(&helper.program.to_string_lossy())];
    command.extend(helper.args_for(target).iter().map(|arg| quote(arg)));
    let command = command.join(" ");
    format!(
        "#!/bin/sh\n\
         # {MARKER} — do not edit.\n\
         # One entry of the contextual menu: sends the selection to one device of\n\
         # the account. Rewritten whenever that list changes, and removed when the\n\
         # component stops.\n\
         #\n\
         # Nautilus runs this with the browsed folder as the working directory and\n\
         # the selected names as arguments; the helper resolves them against it.\n\
         exec {command} \"$@\"\n"
    )
}

/// Single-quotes for `/bin/sh`: inside single quotes everything is literal, and a
/// single quote itself is closed, escaped and reopened.
///
/// Only our own path and the device id go through this today, but the whole point
/// of the surface is that a click runs a shell: a file manager entry must not be a
/// place where any of it becomes a command.
fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::*;

    fn helper() -> HelperCommand {
        HelperCommand {
            program: PathBuf::from("/opt/universallink/universallink-menu"),
            extra_args: vec![],
        }
    }

    fn target(id: &str, name: &str) -> Target {
        Target {
            device_id: id.into(),
            name: name.into(),
            platform: "linux".into(),
        }
    }

    fn names_of(targets: &[Target]) -> Vec<String> {
        file_names(targets)
    }

    #[test]
    fn one_file_per_target_named_after_the_device() {
        assert_eq!(
            names_of(&[target("d_1", "PC A"), target("d_2", "Le Mac")]),
            ["PC A", "Le Mac"]
        );
    }

    /// A file name cannot hold a separator and must not start with a dot: the
    /// first is impossible, the second would make the entry invisible.
    #[test]
    fn a_name_that_cannot_be_a_file_name_is_repaired() {
        assert_eq!(names_of(&[target("d_1", "Bureau/Salon")]), ["Bureau-Salon"]);
        assert_eq!(names_of(&[target("d_1", ".maison")]), ["maison"]);
        assert_eq!(names_of(&[target("d_1", "  PC A  ")]), ["PC A"]);
        assert_eq!(names_of(&[target("d_1", "PC\nA")]), ["PC A"]);
        // Nothing usable left: the id is always a valid, unique file name.
        assert_eq!(names_of(&[target("d_abcd", "...")]), ["d_abcd"]);
        assert_eq!(names_of(&[target("d_abcd", "  ")]), ["d_abcd"]);
        assert_eq!(names_of(&[target("d_abcd", "/")]), ["-"]);
    }

    /// A name is bounded in BYTES, so a long non-ASCII one has to be cut — and cut
    /// on a character boundary, because slicing a `str` anywhere else PANICS. In a
    /// surface that means the entries are never written at all (the applier catches
    /// the panic and retries it for ever), so the interesting case is the one where
    /// the budget does not happen to land on a boundary: an ASCII character
    /// followed by three-byte ones puts every boundary at `1 + 3k`, and the budget
    /// is not one of those.
    #[test]
    fn a_very_long_name_is_truncated_on_a_character_boundary() {
        assert!(
            !("x".to_string() + &"あ".repeat(400)).is_char_boundary(BASE_BUDGET),
            "this test only means something if the naive cut would be mid-character"
        );
        for long in [
            "é".repeat(400),
            "x".to_string() + &"あ".repeat(400),
            "🇫🇷".repeat(200),
        ] {
            let names = names_of(&[target("d_1", &long)]);
            assert!(names[0].len() <= BASE_BUDGET, "{} bytes", names[0].len());
            assert!(
                long.starts_with(&names[0]),
                "{:?} is not a prefix",
                names[0]
            );
        }
    }

    /// The collision that silently loses an entry: same name, two devices, one
    /// file. BOTH get told apart.
    #[test]
    fn two_devices_with_the_same_name_get_two_distinct_entries() {
        let names = names_of(&[target("d_aaaa1111", "PC"), target("d_bbbb2222", "PC")]);
        assert_eq!(names, ["PC (1111)", "PC (2222)"]);
        assert_ne!(names[0], names[1]);
    }

    /// Even when the disambiguated form is itself taken — a device really called
    /// "PC (1111)" — every target keeps a file of its own.
    #[test]
    fn a_name_that_collides_with_a_disambiguated_one_still_gets_its_own_file() {
        let names = names_of(&[
            target("d_aaaa1111", "PC"),
            target("d_bbbb1111", "PC"),
            target("d_cccc3333", "PC (1111)"),
        ]);
        assert_eq!(names.len(), 3);
        assert_eq!(
            names.iter().collect::<HashSet<_>>().len(),
            3,
            "two devices share an entry: {names:?}"
        );
    }

    #[test]
    fn the_body_only_names_the_device_id_so_a_rename_moves_a_file() {
        let one = script(&helper(), &target("d_1", "PC A"));
        let two = script(&helper(), &target("d_1", "Renamed"));
        assert_eq!(one, two);
        assert!(
            one.contains(
                "exec '/opt/universallink/universallink-menu' '--send' 'd_1' '--' \"$@\"\n"
            ),
            "{one}"
        );
        assert!(one.starts_with("#!/bin/sh\n"));
        assert!(one.contains(MARKER));
    }

    #[test]
    fn a_single_quote_in_our_own_path_is_closed_escaped_and_reopened() {
        let helper = HelperCommand {
            program: PathBuf::from("/opt/it's here/menu"),
            extra_args: vec![],
        };
        assert!(
            script(&helper, &target("d_1", "PC")).contains(r"exec '/opt/it'\''s here/menu' "),
            "{}",
            script(&helper, &target("d_1", "PC"))
        );
    }

    // -----------------------------------------------------------------------
    // The surface against a real directory, and a real /bin/sh.
    // -----------------------------------------------------------------------

    /// A stand-in for the courier that records the argv it was given, one file per
    /// argument so that a name containing a newline is still readable back.
    fn recorder(dir: &Path) -> PathBuf {
        let path = dir.join("recorder.sh");
        std::fs::write(
            &path,
            "#!/bin/sh\ni=0\nfor a in \"$@\"; do i=$((i+1)); printf '%s' \"$a\" > \"$OUT.$i\"; done\nprintf '%s' \"$i\" > \"$OUT.n\"\n",
        )
        .expect("write the recorder");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn recorded(out: &Path) -> Vec<String> {
        let count: usize = std::fs::read_to_string(out.with_extension("n"))
            .expect("the recorder did not run")
            .parse()
            .expect("count");
        (1..=count)
            .map(|i| std::fs::read_to_string(format!("{}.{i}", out.display())).expect("argument"))
            .collect()
    }

    /// The click, for real: a `/bin/sh` we did not write parses a script we did,
    /// with a selection built to break it. What reaches the courier must be
    /// exactly the file names Nautilus passed, and nothing must have run.
    #[test]
    fn a_click_hands_the_selection_over_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        std::fs::create_dir(&work).expect("work dir");
        let out = dir.path().join("argv");

        let mut surface = Scripts::new(
            dir.path(),
            HelperCommand {
                program: recorder(dir.path()),
                extra_args: vec![],
            },
        );
        surface.apply(&[target("d_1", "PC A")]).expect("apply");

        let selection = [
            "plain.txt",
            "two words.txt",
            "$(touch pwned).txt",
            "`touch pwned2`.txt",
            "it's here.txt",
            "line\nbreak.txt",
            "-r",
            "100%.txt",
        ];
        let status = Command::new(surface.dir().join("PC A"))
            .current_dir(&work)
            .env("OUT", &out)
            .args(selection)
            .status()
            .expect("run the entry");
        assert!(status.success(), "the entry failed: {status:?}");

        let mut expected = vec!["--send".to_string(), "d_1".into(), "--".into()];
        expected.extend(selection.iter().map(|s| (*s).to_string()));
        assert_eq!(recorded(&out), expected);
        assert!(!work.join("pwned").exists(), "a file name was executed");
        assert!(!work.join("pwned2").exists(), "a file name was executed");
    }

    #[test]
    fn applying_writes_an_executable_entry_per_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Scripts::new(dir.path(), helper());
        assert_eq!(
            surface.dir(),
            dir.path()
                .join("nautilus")
                .join("scripts")
                .join("UniversalLink")
        );

        surface
            .apply(&[target("d_1", "PC A"), target("d_2", "PC B")])
            .expect("apply");

        for name in ["PC A", "PC B"] {
            let path = surface.dir().join(name);
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o755,
                "Nautilus ignores a non-executable script"
            );
        }
    }

    /// The absolute contract: after `apply`, the directory shows exactly the
    /// targets — a device that went offline or was renamed leaves nothing behind.
    #[test]
    fn entries_that_are_no_longer_targets_are_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Scripts::new(dir.path(), helper());

        surface
            .apply(&[target("d_1", "PC A"), target("d_2", "PC B")])
            .expect("apply");
        // d_2 went offline, and d_1 was renamed: one entry to drop, one to move.
        surface.apply(&[target("d_1", "Bureau")]).expect("reapply");

        assert_eq!(listing(surface.dir()), ["Bureau"]);
    }

    #[test]
    fn an_empty_list_removes_the_submenu_entirely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Scripts::new(dir.path(), helper());

        surface.apply(&[target("d_1", "PC A")]).expect("apply");
        surface.apply(&[]).expect("apply empty");
        assert!(!surface.dir().exists(), "the submenu outlived its entries");
        // And the startup render, which always applies an empty list, must not
        // fail on a directory that was never created.
        surface.apply(&[]).expect("apply empty twice");
    }

    /// Pruning enumerates a directory and deletes from it. A file we did not write
    /// is never ours to remove — and it keeps the directory alive.
    #[test]
    fn a_file_someone_else_left_here_is_never_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Scripts::new(dir.path(), helper());
        surface.apply(&[target("d_1", "PC A")]).expect("apply");

        let theirs = surface.dir().join("their own script");
        std::fs::write(&theirs, "#!/bin/sh\necho mine\n").expect("write");

        surface.apply(&[]).expect("apply empty");
        assert!(theirs.exists(), "someone else's script was deleted");
        assert!(
            !surface.dir().join("PC A").exists(),
            "our own entry should be gone"
        );
        assert!(
            surface.dir().exists(),
            "the directory still holds their file"
        );
    }

    /// An entry left by a previous run — a device that is no longer online, or a
    /// name from an older version — is ours and goes.
    #[test]
    fn an_entry_from_a_previous_run_is_swept_at_startup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Scripts::new(dir.path(), helper());
        std::fs::create_dir_all(surface.dir()).expect("dir");
        std::fs::write(
            surface.dir().join("Ghost"),
            format!("#!/bin/sh\n# {MARKER}\nexec /nowhere\n"),
        )
        .expect("write");

        // Exactly what the applier does first.
        surface.apply(&[]).expect("startup render");
        assert!(
            !surface.dir().exists(),
            "a stale entry survived: {:?}",
            dir.path()
        );
    }

    /// The stale-artifact rule one level up: Nautilus reads `scripts/` itself as a
    /// menu too, so an entry we wrote there before the submenu existed is a live item
    /// frozen on an old device list. It goes; the user's own script and their own
    /// folder next to it do not, whatever they are called.
    #[test]
    fn an_entry_of_ours_from_a_flat_layout_is_swept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Scripts::new(dir.path(), helper());
        let scripts = surface.dir().parent().expect("scripts/").to_path_buf();
        std::fs::create_dir_all(&scripts).expect("mkdir");

        let stale = scripts.join("Send to Ghost (UniversalLink)");
        std::fs::write(&stale, format!("#!/bin/sh\n# {MARKER}\nexec /nowhere\n")).expect("write");
        let theirs = scripts.join("their own script");
        std::fs::write(&theirs, "#!/bin/sh\necho mine\n").expect("write");
        let their_folder = scripts.join("Their folder");
        std::fs::create_dir(&their_folder).expect("mkdir");

        surface.apply(&[target("d_1", "PC A")]).expect("apply");
        assert!(!stale.exists(), "a flat entry of ours survived a render");
        assert!(surface.dir().join("PC A").exists(), "our entry is missing");

        surface.apply(&[]).expect("apply empty");
        assert!(theirs.exists(), "someone else's script was deleted");
        assert!(their_folder.exists(), "someone else's folder was deleted");
        assert!(!surface.dir().exists(), "our own directory should be gone");
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}
