// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Dolphin's context menu, as a KDE ServiceMenu.
//!
//! One `.desktop` file with one `[Desktop Action …]` per target, listed in
//! `Actions=`. KIO reads the file on every right click, so the whole surface is a
//! single atomic write — and `apply(&[])` is a single unlink.
//!
//! # What KIO requires, and why each line is here
//!
//! Read off `KFileItemActionsPrivate` (kio/src/widgets/kfileitemactions.cpp)
//! rather than guessed, because a wrong key means no menu at all and nothing to
//! debug:
//! - `MimeType=all/all;` — `checkTypesMatch` gives up when both `MimeType` and
//!   `ServiceTypes` are empty, and `all/all` is the pattern its matcher treats as
//!   "anything", directories included (`all/allfiles` would exclude folders,
//!   which we can send since #21).
//! - `X-KDE-Protocol=file` — `shouldDisplayServiceMenu` hides the entry unless the
//!   items come from that protocol. We need real local paths: everything else
//!   (`sftp:`, `mtp:`, a search result) would have KIO either stage a temporary
//!   copy or hand us something the Core cannot open.
//! - `X-KDE-Submenu` — decision 3's submenu, one entry per device inside it.
//! - `X-KDE-Priority=TopLevel` — a request for prominence, not a guarantee: in
//!   current KIO it orders us ahead of the other service menus, and whether the
//!   whole lot ends up directly in the context menu or folded into a generic
//!   "Actions" submenu depends on how many of them the user has (`addServiceActionsTo`
//!   makes that submenu past three). Nothing to work around — either way the
//!   entries are one level from where they were.
//! - the executable bit — what KDE's own documentation tells authors to set.
//!
//! The `Name=` of each action is the only place a *device name* reaches this file,
//! and it is the reason [`escape_value`] exists: a Desktop Entry is a line-based
//! format, so a name carrying a newline would otherwise close `Name=` and let the
//! rest of it open any key it likes — `Exec=` included, which is a command line
//! the desktop runs on click. KIO escapes the `&` mnemonic itself
//! (`createActionForService`), so the label must NOT be pre-escaped for that.

use std::io;
use std::path::{Path, PathBuf};

use super::{MARKER, label_of, remove_if_present, write_if_changed};
use crate::surface::{HelperCommand, MenuSurface, Target};

/// Distinctive on purpose: KIO deduplicates service menus by FILE NAME across
/// every data directory (`serviceMenuFilePaths`), so a generic name risks being
/// shadowed by a distribution's own file — or shadowing it.
const FILE_NAME: &str = "universallink-send.desktop";
/// Decision 3: a submenu where the surface allows one.
const SUBMENU: &str = "UniversalLink";
/// A stock freedesktop icon name, so the entries have an icon on a plain install:
/// we install no icon theme of our own on Linux (the AppImage keeps its icon
/// inside the bundle). A name a theme does not carry simply shows no icon.
const ICON: &str = "document-send";
/// Executable, per KDE's documentation for service menus.
const MODE: u32 = 0o755;

/// The KDE ServiceMenu surface.
pub struct ServiceMenu {
    path: PathBuf,
    helper: HelperCommand,
}

impl ServiceMenu {
    pub fn new(data_home: &Path, helper: HelperCommand) -> ServiceMenu {
        ServiceMenu {
            path: data_home.join("kio").join("servicemenus").join(FILE_NAME),
            helper,
        }
    }

    /// Where the artifact lives. For the tests, and for looking at a real desktop
    /// by hand.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl MenuSurface for ServiceMenu {
    fn name(&self) -> &'static str {
        "kde-servicemenu"
    }

    fn apply(&mut self, targets: &[Target]) -> io::Result<()> {
        if targets.is_empty() {
            // Removed, not emptied: a file with no action is still one KIO opens
            // and parses on every right click, and "no manager, no trace" is the
            // rule.
            return remove_if_present(&self.path);
        }
        write_if_changed(&self.path, &desktop_file(&self.helper, targets), MODE)
    }
}

/// The whole file, in `targets`' order.
fn desktop_file(helper: &HelperCommand, targets: &[Target]) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("[Desktop Entry]\n");
    out.push_str(&format!("# {MARKER} — do not edit.\n"));
    out.push_str("# Rewritten whenever the account's device list changes, and removed when\n");
    out.push_str("# the component stops. Anything added here is lost.\n");
    out.push_str("Type=Service\n");
    // Required by KIO 5.84 and older, and still the key the compatibility scan of
    // `kservices5` filters on. Ignored by the new location, which is where we
    // write: `checkTypesMatch` prefers `MimeType` whenever it is set.
    out.push_str("ServiceTypes=KonqPopupMenu/Plugin\n");
    out.push_str("MimeType=all/all;\n");
    out.push_str("X-KDE-Protocol=file\n");
    out.push_str(&format!("X-KDE-Submenu={SUBMENU}\n"));
    out.push_str("X-KDE-Priority=TopLevel\n");

    out.push_str("Actions=");
    for index in 0..targets.len() {
        out.push_str(&action_name(index));
        out.push(';');
    }
    out.push('\n');

    for (index, target) in targets.iter().enumerate() {
        out.push_str(&format!("\n[Desktop Action {}]\n", action_name(index)));
        out.push_str(&format!("Name={}\n", escape_value(label_of(&target.name))));
        out.push_str(&format!("Icon={ICON}\n"));
        out.push_str(&format!("Exec={}\n", exec_value(helper, target)));
    }
    out
}

/// The name of one action: a group name and a config key, so it has to be an
/// identifier. Ours, positional, never the device's — a device name is whatever
/// the user typed, and `Actions=` is a `;`-separated list.
fn action_name(index: usize) -> String {
    format!("universallink-send-{index}")
}

/// Escapes a value for the Desktop Entry format: the spec's general string rule.
///
/// This is what keeps a device name from becoming a key. Applied to `Exec=` too,
/// AFTER its arguments are quoted — that order is what turns one literal
/// backslash into the four the spec calls for.
fn escape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            _ => out.push(c),
        }
    }
    out
}

/// The `Exec=` value: our own program, the fixed arguments, then `%F`.
///
/// `%F` comes last and is deliberately NOT escaped — it is a field code, not an
/// argument, and KIO replaces it with the selected paths (one argument each). The
/// spec forbids a field code inside a quoted argument, which is why it stands
/// alone.
fn exec_value(helper: &HelperCommand, target: &Target) -> String {
    let mut args = vec![helper.program.to_string_lossy().into_owned()];
    args.extend(helper.args_for(target));
    let line = args
        .iter()
        .map(|arg| exec_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    escape_value(&format!("{line} %F"))
}

/// One argument of an `Exec=` line, quoted per the Desktop Entry spec.
///
/// The spec's reserved set, plus every other control character: quoting more than
/// required is always allowed, and a stray carriage return in a path is not worth
/// reasoning about.
fn exec_arg(arg: &str) -> String {
    const RESERVED: &[char] = &[
        ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(',
        ')', '`',
    ];
    let reserved = |c: char| RESERVED.contains(&c) || c.is_control();

    // A literal percent is `%%` quoted or not: it is what introduces a field code,
    // so an unescaped one would be swallowed — or expanded to something else.
    if !arg.is_empty() && !arg.chars().any(reserved) {
        return arg.replace('%', "%%");
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        match c {
            // The spec: inside double quotes, these four are escaped with an
            // additional backslash. That backslash is then itself escaped by the
            // file's string rule (`escape_value`), which is the double escaping
            // the spec spells out.
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
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

    fn lines_of(content: &str) -> Vec<&str> {
        content.lines().collect()
    }

    #[test]
    fn the_file_declares_one_action_per_target_and_a_group_for_each() {
        let content = desktop_file(&helper(), &[target("d_1", "PC A"), target("d_2", "Le Mac")]);
        let lines = lines_of(&content);

        assert_eq!(lines[0], "[Desktop Entry]");
        assert!(
            lines.contains(&"Actions=universallink-send-0;universallink-send-1;"),
            "{content}"
        );
        assert!(
            lines.contains(&"[Desktop Action universallink-send-0]"),
            "{content}"
        );
        assert!(lines.contains(&"Name=PC A"), "{content}");
        assert!(
            lines.contains(&"[Desktop Action universallink-send-1]"),
            "{content}"
        );
        assert!(lines.contains(&"Name=Le Mac"), "{content}");
        assert!(
            lines.contains(&"Exec=/opt/universallink/universallink-menu --send d_1 -- %F"),
            "{content}"
        );
        // The keys that decide whether the entry appears at all.
        for required in [
            "MimeType=all/all;",
            "X-KDE-Protocol=file",
            "X-KDE-Submenu=UniversalLink",
        ] {
            assert!(lines.contains(&required), "missing {required}: {content}");
        }
        assert!(
            content.contains(MARKER),
            "the artifact must be recognizable"
        );
    }

    /// The injection this whole module is careful about: a device name is
    /// user-controlled text landing in a line-based format whose keys include one
    /// the desktop EXECUTES. A newline in it must stay inside the value.
    #[test]
    fn a_device_name_cannot_open_a_key_of_its_own() {
        let content = desktop_file(
            &helper(),
            &[target(
                "d_1",
                "PC\nExec=/bin/sh -c \"rm -rf ~\"\nName=Innocent",
            )],
        );

        let execs: Vec<&str> = content.lines().filter(|l| l.starts_with("Exec=")).collect();
        assert_eq!(
            execs,
            ["Exec=/opt/universallink/universallink-menu --send d_1 -- %F"],
            "the name smuggled in a key: {content}"
        );
        let names: Vec<&str> = content.lines().filter(|l| l.starts_with("Name=")).collect();
        assert_eq!(
            names,
            // A double quote needs no escaping in a VALUE (only inside an `Exec`
            // argument), so it stays as typed; the newlines are what mattered.
            [r#"Name=PC\nExec=/bin/sh -c "rm -rf ~"\nName=Innocent"#],
            "{content}"
        );
        // And no line of the file is anything but a comment, a group header or a
        // key we chose.
        for line in content.lines().filter(|l| !l.is_empty()) {
            assert!(
                line.starts_with('#')
                    || line.starts_with('[')
                    || line.split('=').next().is_some_and(|key| [
                        "Type",
                        "ServiceTypes",
                        "MimeType",
                        "X-KDE-Protocol",
                        "X-KDE-Submenu",
                        "X-KDE-Priority",
                        "Actions",
                        "Name",
                        "Icon",
                        "Exec"
                    ]
                    .contains(&key)),
                "unexpected line {line:?} in {content}"
            );
        }
    }

    /// A name is trimmed, because a Desktop Entry value loses its surrounding
    /// whitespace on the way back in: keeping it would mean the label we write and
    /// the label KDE shows are not the same string.
    #[test]
    fn a_name_is_trimmed_and_its_tabs_survive_as_escapes() {
        let content = desktop_file(&helper(), &[target("d_1", "  PC\tA  ")]);
        assert!(content.contains("Name=PC\\tA\n"), "{content}");
    }

    /// The spec's quoting rules, pinned literally. An argument with no reserved
    /// character is emitted bare — which is what keeps the file readable for
    /// whoever debugs a desktop by hand.
    #[test]
    fn exec_arguments_are_quoted_exactly_as_the_spec_says() {
        assert_eq!(exec_arg("--send"), "--send");
        assert_eq!(
            exec_arg("/opt/universallink-menu"),
            "/opt/universallink-menu"
        );
        assert_eq!(exec_arg(""), r#""""#);
        assert_eq!(exec_arg("/opt/My Apps/menu"), r#""/opt/My Apps/menu""#);
        // The four the spec singles out inside double quotes.
        assert_eq!(exec_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(exec_arg("a`b"), r#""a\`b""#);
        assert_eq!(exec_arg("a$b"), r#""a\$b""#);
        assert_eq!(exec_arg(r"a\b"), r#""a\\b""#);
        // A percent is the field-code introducer, quoted or not.
        assert_eq!(exec_arg("100%"), "100%%");
        assert_eq!(exec_arg("100% sure"), r#""100%% sure""#);
    }

    /// The double escaping the spec spells out: one literal backslash in an
    /// argument is four in the file. `exec_arg` writes two (the Exec quoting rule),
    /// `escape_value` doubles each of them (the string rule).
    #[test]
    fn a_backslash_in_our_own_path_ends_up_as_four() {
        let helper = HelperCommand {
            program: PathBuf::from(r"/opt/we\ird/menu"),
            extra_args: vec![],
        };
        assert_eq!(
            exec_value(&helper, &target("d_1", "PC")),
            r#""/opt/we\\\\ird/menu" --send d_1 -- %F"#
        );
    }

    /// The channel override a live test needs must reach the courier: it is part
    /// of the command line, before the mode.
    #[test]
    fn the_helper_prefix_is_kept_in_the_command_line() {
        let helper = HelperCommand {
            program: PathBuf::from("/opt/menu"),
            extra_args: vec!["--channel".into(), "/tmp/t.sock".into()],
        };
        assert_eq!(
            exec_value(&helper, &target("d_1", "PC")),
            "/opt/menu --channel /tmp/t.sock --send d_1 -- %F"
        );
    }

    #[test]
    fn applying_a_list_writes_an_executable_file_and_an_empty_one_removes_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = ServiceMenu::new(dir.path(), helper());
        let path = surface.path().to_path_buf();
        assert_eq!(
            path,
            dir.path()
                .join("kio")
                .join("servicemenus")
                .join("universallink-send.desktop")
        );

        surface.apply(&[target("d_1", "PC A")]).expect("apply");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "KDE wants a service menu executable");
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("Name=PC A")
        );

        // No manager, no entry.
        surface.apply(&[]).expect("apply empty");
        assert!(!path.exists(), "the entry outlived the target list");
        // And doing it again is not an error: the startup render always does.
        surface.apply(&[]).expect("apply empty twice");
    }
}
