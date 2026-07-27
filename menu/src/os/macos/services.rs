// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! One `.workflow` bundle per device in `~/Library/Services`.
//!
//! The shape of the two plists, and why each key is there, is in the parent
//! module's header — along with what was measured on a real macOS to establish it.
//! What is here is the surface: what a bundle is named, what its entry is labelled,
//! and how a directory shared with every other application's services is pruned
//! safely.
//!
//! # The label carries more here than anywhere else
//!
//! On Linux and Windows our entries live in a submenu of our own, so a label only
//! has to name a device. Here they land among everybody else's services, so each one
//! has to say what it does AND that it is ours: `Send to <device> (UniversalLink)`.
//! The suffix is the same convention as the Windows "Send to" shortcuts, for the
//! same reason — an entry sitting in a shared list must be attributable.
//!
//! And the label is not only shown: it is what a service lookup resolves (measured,
//! see the parent header), and two bundles carrying the same one both register. So
//! two devices with a single name are disambiguated with a piece of the device id —
//! BOTH of them, never just the second, because "PC" next to "PC (a1b2)" tells the
//! user nothing about which is which. That is brick 2's Nautilus ladder, applied to
//! a plist string instead of a file name.
//!
//! The bundle's own NAME, by contrast, is free: nothing displays it and no lookup
//! resolves it, so it is built from the device id and never changes when a device is
//! renamed. A rename rewrites one plist in place.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use super::{
    MARKER, MARKER_KEY, is_ours, is_xml_char, notify_services_changed, remove_bundle,
    write_if_changed, xml_escape,
};
use crate::surface::{HelperCommand, MenuSurface, Target};

/// A service bundle is a directory whose name ends in this.
const EXTENSION: &str = ".workflow";
/// And begins with this, so a human looking at `~/Library/Services` can see whose
/// bundles these are without opening one.
const PREFIX: &str = "UniversalLink-";
/// What tells the user, in a menu full of other applications' services, which
/// application put this one there.
const SUFFIX: &str = " (UniversalLink)";
/// Prefix of a bundle's reverse-DNS identifier, spelled like the Core's LaunchAgent
/// (`gui/src/supervise.rs`).
const IDENTIFIER: &str = "org.universallink.menu.";
/// The action the document runs, and its identifier — both read off the real bundle
/// on a real macOS. The engine resolves the action by PATH (measured: a wrong
/// `BundleIdentifier` still ran), but a wrong identifier in a file a human may open
/// would be a lie.
const ACTION_PATH: &str = "/System/Library/Automator/Run Shell Script.action";
const ACTION_NAME: &str = "Run Shell Script";
const ACTION_IDENTIFIER: &str = "com.apple.RunShellScript";
/// `inputMethod`: 1 is "as arguments" (measured — see the parent header). 0 would
/// put the selection on standard input, which the courier does not read.
const AS_ARGUMENTS: u8 = 1;
/// How many CHARACTERS of a device name a label may carry. A plist string is not
/// bounded in bytes the way a file name is; what bounds this is a menu a human has
/// to read.
const NAME_BUDGET: usize = 64;

/// The macOS Services / Quick Actions surface.
pub struct Services {
    dir: PathBuf,
    helper: HelperCommand,
}

/// One bundle, fully rendered: nothing here touches the filesystem, so the whole
/// plan can be tested — and asserted against the system's own plist parser — without
/// a directory.
struct Bundle {
    name: String,
    info: String,
    workflow: String,
}

impl Services {
    pub fn new(services: &Path, helper: HelperCommand) -> Services {
        Services {
            dir: services.to_path_buf(),
            helper,
        }
    }

    /// The directory the bundles live in. For the tests, and for looking at a real
    /// desktop by hand.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes one bundle, reporting whether anything changed on disk.
    ///
    /// The document goes FIRST and the `Info.plist` last, because the plist is what
    /// registers the service: in that order a half-written bundle is an entry that
    /// does not exist yet, and in the other it is an entry whose workflow is
    /// missing — one that shows up in a menu and does nothing. Same reasoning as the
    /// Windows cascade, which writes its children before the parent that names them.
    fn write(&self, bundle: &Bundle) -> io::Result<bool> {
        let contents = self.dir.join(&bundle.name).join("Contents");
        let workflow = write_if_changed(
            &contents.join("Resources").join("document.wflow"),
            &bundle.workflow,
        )?;
        let info = write_if_changed(&contents.join("Info.plist"), &bundle.info)?;
        Ok(workflow || info)
    }

    /// Deletes every bundle of ours that is not in `keep`, reporting whether it
    /// deleted anything.
    ///
    /// This is what makes the surface absolute rather than incremental: a device that
    /// went offline, a device that was renamed into a different bundle name, a bundle
    /// left by a previous version. `~/Library/Services` belongs to every application
    /// that has a service, so anything WITHOUT our marker is left strictly alone —
    /// and the directory itself is never removed, even when we leave it empty.
    fn prune(&self, keep: &HashSet<&str>) -> io::Result<bool> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // Nothing was ever written, or the user has no services directory.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mut removed = false;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if keep.contains(name) || !name.ends_with(EXTENSION) {
                continue;
            }
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) && is_ours(&path) {
                remove_bundle(&path)?;
                removed = true;
            }
        }
        Ok(removed)
    }
}

impl MenuSurface for Services {
    fn name(&self) -> &'static str {
        "macos-services"
    }

    fn apply(&mut self, targets: &[Target]) -> io::Result<()> {
        let wanted = plan(&self.helper, targets)?;
        let mut changed = false;
        for bundle in &wanted {
            changed |= self.write(bundle)?;
        }
        changed |= self.prune(&wanted.iter().map(|b| b.name.as_str()).collect())?;
        if changed {
            // Only when something moved: this asks the system to re-read every
            // application's services, which is not free and not ours to trigger for
            // nothing.
            notify_services_changed();
        }
        Ok(())
    }
}

/// Every bundle to write, in `targets`' order.
fn plan(helper: &HelperCommand, targets: &[Target]) -> io::Result<Vec<Bundle>> {
    let labels = labels(targets);
    let names = bundle_names(targets);
    targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            Ok(Bundle {
                info: info_plist(&identifier(&names[index]), &labels[index]),
                name: names[index].clone(),
                workflow: document(&script(helper, target)?),
            })
        })
        .collect()
}

/// One distinct label per target.
fn labels(targets: &[Target]) -> Vec<String> {
    let bases: Vec<String> = targets.iter().map(base_label).collect();
    let mut used: HashSet<String> = HashSet::new();
    let mut labels = Vec::with_capacity(targets.len());
    for (index, target) in targets.iter().enumerate() {
        let base = &bases[index];
        let shared = bases
            .iter()
            .filter(|other| other.as_str() == base.as_str())
            .count()
            > 1;
        let core = [
            (!shared).then(|| base.clone()),
            Some(format!("{base} ({})", tail(&target.device_id))),
            Some(format!("{base} ({})", target.device_id)),
            // A device id is unique account-wide and short enough to survive the
            // budget, so a free candidate always exists.
            Some(target.device_id.clone()),
            Some(format!("{} ({index})", target.device_id)),
        ]
        .into_iter()
        .flatten()
        .map(|candidate| fit(&candidate, NAME_BUDGET))
        .find(|candidate| !used.contains(candidate))
        .unwrap_or_else(|| format!("{index}"));
        used.insert(core.clone());
        labels.push(format!("Send to {core}{SUFFIX}"));
    }
    labels
}

/// The device name, made into something a menu can show and a plist can hold.
fn base_label(target: &Target) -> String {
    let mut out = String::with_capacity(target.name.len());
    for c in target.name.chars() {
        // Two reasons at once: a control character has no place in a menu label, and
        // XML cannot carry one at all — a single tab in a device name would make the
        // whole Info.plist invalid, and the entry would simply not exist.
        if c.is_control() || !is_xml_char(c) {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    // Trimmed AFTER the mapping, and only then: a name ending in a control character
    // becomes one ending in a space, and trimming first would leave it there.
    let cleaned = fit(out.trim(), NAME_BUDGET);
    if cleaned.is_empty() {
        return target.device_id.clone();
    }
    cleaned
}

/// One distinct bundle name per target, built from the device id so that renaming a
/// device rewrites a plist instead of moving a directory.
fn bundle_names(targets: &[Target]) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::new();
    targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            let mut name = format!("{PREFIX}{}{EXTENSION}", safe_id(&target.device_id));
            // Server-minted ids (`d_` + hex, see `server/src/conn.rs`) cannot collide
            // through `safe_id`. This is here so that the day they can, a device
            // loses its entry loudly rather than by being overwritten in silence.
            if used.contains(&name) {
                name = format!("{PREFIX}{index}-{}{EXTENSION}", safe_id(&target.device_id));
            }
            used.insert(name.clone());
            name
        })
        .collect()
}

/// Reverse-DNS identifier of the bundle called `bundle_name`.
///
/// Derived from the NAME rather than from the device id, so that it is unique whenever
/// the name is: two bundles sharing a `CFBundleIdentifier` is a state LaunchServices
/// has no reason to handle well, and nothing here should be able to produce it.
fn identifier(bundle_name: &str) -> String {
    let stem = bundle_name
        .strip_prefix(PREFIX)
        .unwrap_or(bundle_name)
        .strip_suffix(EXTENSION)
        .unwrap_or(bundle_name);
    format!("{IDENTIFIER}{stem}")
}

/// A device id reduced to what is safe in a file name and in a bundle identifier.
fn safe_id(device_id: &str) -> String {
    device_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect()
}

/// The tail of a device id: enough to tell two same-named devices apart without
/// putting an 18-character identifier in a menu.
fn tail(device_id: &str) -> &str {
    let start = device_id.len().saturating_sub(4);
    device_id.get(start..).unwrap_or(device_id)
}

/// Truncates to `max` CHARACTERS. Slicing a `str` anywhere else panics, and a
/// device name is arbitrary Unicode.
fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars()
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// The bundle's `Info.plist`: what registers the service, what it is called, and
/// the marker that makes it ours to delete.
fn info_plist(identifier: &str, label: &str) -> String {
    let label = xml_escape(label);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>{identifier}</string>
	<key>CFBundleName</key>
	<string>{label}</string>
	<key>CFBundleShortVersionString</key>
	<string>1.0</string>
	<key>NSServices</key>
	<array>
		<dict>
			<key>NSMenuItem</key>
			<dict>
				<key>default</key>
				<string>{label}</string>
			</dict>
			<key>NSMessage</key>
			<string>runWorkflowAsService</string>
			<key>NSSendFileTypes</key>
			<array>
				<string>public.item</string>
			</array>
		</dict>
	</array>
	<key>{MARKER_KEY}</key>
	<string>{MARKER}</string>
</dict>
</plist>
"#,
        identifier = xml_escape(identifier),
    )
}

/// The workflow document: one "Run Shell Script" action, and the service metadata
/// Apple's own file workflows carry.
fn document(script: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AMDocumentVersion</key>
	<string>2</string>
	<key>actions</key>
	<array>
		<dict>
			<key>action</key>
			<dict>
				<key>ActionBundlePath</key>
				<string>{ACTION_PATH}</string>
				<key>ActionName</key>
				<string>{ACTION_NAME}</string>
				<key>ActionParameters</key>
				<dict>
					<key>COMMAND_STRING</key>
					<string>{script}</string>
					<key>CheckedForUserDefaultShell</key>
					<true/>
					<key>inputMethod</key>
					<integer>{AS_ARGUMENTS}</integer>
					<key>shell</key>
					<string>/bin/sh</string>
					<key>source</key>
					<string></string>
				</dict>
				<key>BundleIdentifier</key>
				<string>{ACTION_IDENTIFIER}</string>
			</dict>
		</dict>
	</array>
	<key>connectors</key>
	<dict/>
	<key>workflowMetaData</key>
	<dict>
		<key>serviceApplicationBundleID</key>
		<string></string>
		<key>serviceInputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject</string>
		<key>serviceOutputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>serviceProcessesInput</key>
		<integer>0</integer>
		<key>workflowTypeIdentifier</key>
		<string>com.apple.Automator.servicesMenu</string>
	</dict>
</dict>
</plist>
"#,
        script = xml_escape(script),
    )
}

/// The script one entry runs. Only the device *id* appears in it, so two labels of
/// the same device produce the same script — a rename never rewrites the document.
///
/// Fails if our own command line holds a character no XML document can carry.
/// Mangling it would write an entry naming a program that does not exist — a menu
/// item that fails at every click, silently — so this is refused instead, and the
/// applier logs it (and retries, backing off to a minute) rather than shipping a
/// broken menu.
fn script(helper: &HelperCommand, target: &Target) -> io::Result<String> {
    let mut command = vec![quote(&helper.program.to_string_lossy())];
    command.extend(helper.args_for(target).iter().map(|arg| quote(arg)));
    let command = command.join(" ");
    if let Some(bad) = command.chars().find(|c| !is_xml_char(*c)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "our own command line holds {bad:?}, which no XML document can carry, \
                 so no workflow could name it: {command}"
            ),
        ));
    }
    Ok(format!(
        "# {MARKER} — do not edit.\n\
         # One entry of the contextual menu: sends the selection to one device of\n\
         # the account. Rewritten whenever that list changes, and removed when the\n\
         # component stops.\n\
         #\n\
         # The workflow passes the whole selection as arguments, absolute; everything\n\
         # here is single-quoted, so a file named `$(rm -rf ~)` is an argument and\n\
         # nothing else. POSIX, so it does not matter which shell runs it.\n\
         exec {command} \"$@\"\n"
    ))
}

/// Single-quotes for `/bin/sh`: inside single quotes everything is literal, and a
/// single quote itself is closed, escaped and reopened. Brick 2's answer, unchanged
/// — what is new is that the result then has to survive being XML.
fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn helper() -> HelperCommand {
        HelperCommand {
            program: PathBuf::from("/Applications/UniversalLink.app/universallink-menu"),
            extra_args: vec![],
        }
    }

    fn target(id: &str, name: &str) -> Target {
        Target {
            device_id: id.into(),
            name: name.into(),
            platform: "macos".into(),
        }
    }

    /// Reads one value out of a plist with the system's own parser. The point of the
    /// whole surface is that macOS can read what we write, so the oracle has to be
    /// macOS's reader and not a second writer of ours.
    fn plist_value(path: &Path, key: &str) -> String {
        let out = Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(path)
            .output()
            .expect("run plutil");
        assert!(
            out.status.success(),
            "plutil could not read {key} from {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    fn lint(path: &Path) {
        let out = Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(path)
            .output()
            .expect("run plutil");
        assert!(
            out.status.success(),
            "{} is not a plist the system can parse: {}",
            path.display(),
            String::from_utf8_lossy(&out.stdout)
        );
    }

    fn labels_of(targets: &[Target]) -> Vec<String> {
        labels(targets)
    }

    // -----------------------------------------------------------------------
    // Names and labels.
    // -----------------------------------------------------------------------

    #[test]
    fn a_bundle_is_named_after_the_device_id_and_the_label_after_its_name() {
        let targets = [target("d_1", "PC A"), target("d_2", "Le Mac")];
        assert_eq!(
            bundle_names(&targets),
            ["UniversalLink-d_1.workflow", "UniversalLink-d_2.workflow"]
        );
        assert_eq!(
            labels_of(&targets),
            [
                "Send to PC A (UniversalLink)",
                "Send to Le Mac (UniversalLink)"
            ]
        );
    }

    /// The identifier follows the bundle NAME, so two bundles cannot share one — a
    /// device id that had to be disambiguated into a different name gets a different
    /// identifier with it.
    #[test]
    fn the_identifier_follows_the_bundle_name() {
        assert_eq!(
            identifier("UniversalLink-d_1.workflow"),
            "org.universallink.menu.d_1"
        );
        assert_ne!(
            identifier("UniversalLink-d_1.workflow"),
            identifier("UniversalLink-1-d_1.workflow")
        );
    }

    /// The bundle name must not move when a device is renamed: that would delete a
    /// registered service and register another, for a label change.
    #[test]
    fn renaming_a_device_leaves_its_bundle_where_it_is() {
        assert_eq!(
            bundle_names(&[target("d_1", "Before")]),
            bundle_names(&[target("d_1", "After")])
        );
    }

    /// Two devices, one name: the system registers both labels and resolves a lookup
    /// by label, so both have to be told apart.
    #[test]
    fn two_devices_with_the_same_name_get_two_distinct_labels() {
        let labels = labels_of(&[target("d_aaaa1111", "PC"), target("d_bbbb2222", "PC")]);
        assert_eq!(
            labels,
            [
                "Send to PC (1111) (UniversalLink)",
                "Send to PC (2222) (UniversalLink)"
            ]
        );
    }

    /// Even when the disambiguated form is itself taken — a device really called
    /// "PC (1111)" — every target keeps a label of its own.
    #[test]
    fn a_name_that_collides_with_a_disambiguated_one_still_gets_its_own_label() {
        let labels = labels_of(&[
            target("d_aaaa1111", "PC"),
            target("d_bbbb1111", "PC"),
            target("d_cccc3333", "PC (1111)"),
        ]);
        assert_eq!(labels.len(), 3);
        assert_eq!(
            labels.iter().collect::<HashSet<_>>().len(),
            3,
            "two devices share an entry: {labels:?}"
        );
    }

    #[test]
    fn a_name_with_nothing_showable_in_it_falls_back_to_the_id() {
        assert_eq!(
            labels_of(&[target("d_abcd", "   ")]),
            ["Send to d_abcd (UniversalLink)"]
        );
        assert_eq!(
            labels_of(&[target("d_abcd", "\u{1}\u{2}")]),
            ["Send to d_abcd (UniversalLink)"]
        );
    }

    /// Surrounding whitespace goes: a label is compared with itself on every
    /// re-render, and one that only differs by a space the menu does not show would
    /// rewrite a bundle for nothing.
    #[test]
    fn a_label_loses_the_whitespace_around_the_name() {
        assert_eq!(
            labels_of(&[target("d_1", "  PC A \t")]),
            ["Send to PC A (UniversalLink)"]
        );
    }

    /// A device id becomes a directory name, so a separator in one would write
    /// OUTSIDE the services directory. Server-minted ids cannot hold one; this pins
    /// that nothing here would let one through if they could. A dot survives — it is
    /// only ever part of a longer name, never a component of its own.
    #[test]
    fn an_id_that_could_escape_the_directory_is_reduced_to_one_name() {
        let names = bundle_names(&[target("d_../../evil", "PC")]);
        assert_eq!(names, ["UniversalLink-d_.._.._evil.workflow"]);
        assert!(
            !names[0].contains('/'),
            "{:?} still holds a separator",
            names[0]
        );
        assert_eq!(identifier(&names[0]), "org.universallink.menu.d_.._.._evil");
    }

    /// A long name is cut on a CHARACTER boundary — slicing a `str` anywhere else
    /// panics, which in a surface means the entries are never written at all.
    #[test]
    fn a_very_long_name_is_cut_to_something_a_menu_can_show() {
        for long in ["é".repeat(400), "あ".repeat(400), "🇫🇷".repeat(200)] {
            let labels = labels_of(&[target("d_1", &long)]);
            let core = labels[0]
                .strip_prefix("Send to ")
                .and_then(|l| l.strip_suffix(SUFFIX))
                .expect("the label keeps its shape");
            assert_eq!(core.chars().count(), NAME_BUDGET, "{core:?}");
            assert!(long.starts_with(core), "{core:?} is not a prefix");
        }
    }

    /// The interesting half of the collision ladder under truncation: two names that
    /// only differ past the budget would truncate to one label, so the ladder has to
    /// carry on to something that cannot collide — and what it carries on to has to
    /// respect the budget too, which is the part the base name being bounded does not
    /// give for free.
    #[test]
    fn two_names_that_differ_only_past_the_budget_still_get_two_bounded_labels() {
        let long = "x".repeat(NAME_BUDGET);
        let labels = labels_of(&[
            target("d_aaaa1111", &format!("{long}-one")),
            target("d_bbbb2222", &format!("{long}-two")),
        ]);
        assert_ne!(labels[0], labels[1], "{labels:?}");
        for label in &labels {
            let core = label
                .strip_prefix("Send to ")
                .and_then(|l| l.strip_suffix(SUFFIX))
                .expect("the label keeps its shape");
            assert!(
                core.chars().count() <= NAME_BUDGET,
                "a disambiguated label outgrew the budget: {core:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // What the system parses.
    // -----------------------------------------------------------------------

    /// Both plists, read by `plutil`: valid, and carrying the label, the marker and
    /// the script where the system looks for them.
    #[test]
    fn the_plists_are_ones_the_system_can_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Services::new(dir.path(), helper());
        surface.apply(&[target("d_42", "PC A")]).expect("apply");

        let bundle = dir.path().join("UniversalLink-d_42.workflow");
        let info = bundle.join("Contents").join("Info.plist");
        let document = bundle
            .join("Contents")
            .join("Resources")
            .join("document.wflow");
        lint(&info);
        lint(&document);

        assert_eq!(
            plist_value(&info, "NSServices.0.NSMenuItem.default"),
            "Send to PC A (UniversalLink)"
        );
        assert_eq!(
            plist_value(&info, "NSServices.0.NSMessage"),
            "runWorkflowAsService"
        );
        assert_eq!(
            plist_value(&info, "NSServices.0.NSSendFileTypes.0"),
            "public.item"
        );
        assert_eq!(plist_value(&info, MARKER_KEY), MARKER);
        assert_eq!(
            plist_value(&info, "CFBundleIdentifier"),
            "org.universallink.menu.d_42"
        );
        assert_eq!(
            plist_value(&document, "actions.0.action.ActionParameters.inputMethod"),
            "1"
        );
        assert!(
            plist_value(
                &document,
                "actions.0.action.ActionParameters.COMMAND_STRING"
            )
            .contains("'--send' 'd_42' '--' \"$@\""),
            "the script does not carry the device id"
        );
    }

    /// A device name full of markup: what the menu must show is that text, and what
    /// the plist must stay is valid.
    #[test]
    fn a_name_made_of_markup_survives_as_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Services::new(dir.path(), helper());
        let hostile = r#"<PC> & "Co" 'x'"#;
        surface.apply(&[target("d_1", hostile)]).expect("apply");

        let info = dir
            .path()
            .join("UniversalLink-d_1.workflow")
            .join("Contents")
            .join("Info.plist");
        lint(&info);
        assert_eq!(
            plist_value(&info, "NSServices.0.NSMenuItem.default"),
            format!("Send to {hostile} (UniversalLink)")
        );
    }

    /// A tab in a device name would make the document invalid if it went in raw —
    /// XML 1.0 allows a tab, but a menu label with one is not a label. The plist has
    /// to stay readable either way.
    #[test]
    fn a_control_character_in_a_name_becomes_a_space() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Services::new(dir.path(), helper());
        surface
            .apply(&[target("d_1", "PC\u{1}A\tB\nC")])
            .expect("apply");

        let info = dir
            .path()
            .join("UniversalLink-d_1.workflow")
            .join("Contents")
            .join("Info.plist");
        lint(&info);
        assert_eq!(
            plist_value(&info, "NSServices.0.NSMenuItem.default"),
            "Send to PC A B C (UniversalLink)"
        );
    }

    /// Our own path goes through BOTH layers: single-quoted for the shell, then
    /// escaped as XML. What must come out of the plist is the path itself.
    #[test]
    fn our_own_path_survives_the_shell_and_the_xml_layer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Services::new(
            dir.path(),
            HelperCommand {
                program: PathBuf::from("/Apps/it's <mine> & yours/universallink-menu"),
                extra_args: vec![],
            },
        );
        surface.apply(&[target("d_1", "PC")]).expect("apply");

        let document = dir
            .path()
            .join("UniversalLink-d_1.workflow")
            .join("Contents")
            .join("Resources")
            .join("document.wflow");
        lint(&document);
        let script = plist_value(
            &document,
            "actions.0.action.ActionParameters.COMMAND_STRING",
        );
        assert!(
            script.contains(r"exec '/Apps/it'\''s <mine> & yours/universallink-menu' "),
            "{script}"
        );
    }

    /// The one character no plist can carry. Writing the entry anyway would name a
    /// program that does not exist; writing a mangled path would too.
    #[test]
    fn a_path_no_xml_document_can_carry_is_refused() {
        let mut surface = Services::new(
            Path::new("/nonexistent"),
            HelperCommand {
                program: PathBuf::from("/Apps/bell\u{7}/universallink-menu"),
                extra_args: vec![],
            },
        );
        let err = surface
            .apply(&[target("d_1", "PC")])
            .expect_err("a path with a control character must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("no XML document can carry"));
    }

    #[test]
    fn the_script_names_the_id_and_not_the_label() {
        let one = script(&helper(), &target("d_1", "PC A")).expect("script");
        let two = script(&helper(), &target("d_1", "Renamed")).expect("script");
        assert_eq!(one, two);
        assert!(one.contains(MARKER));
        assert!(
            one.contains(
                "exec '/Applications/UniversalLink.app/universallink-menu' '--send' 'd_1' '--' \"$@\"\n"
            ),
            "{one}"
        );
    }

    // -----------------------------------------------------------------------
    // The click, through a real shell.
    // -----------------------------------------------------------------------

    /// A stand-in for the courier that records the argv it was given, one file per
    /// argument so a name containing a newline is still readable back.
    fn recorder(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

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

    /// The click, for real: the script is taken back out of the document by the
    /// system's own plist parser and handed to a `/bin/sh` we did not write, the way
    /// the workflow engine hands it over — the selection as arguments, `$0` set to
    /// `-` (both measured on a real macOS). What reaches the courier must be exactly
    /// the paths, and nothing must have run.
    #[test]
    fn a_click_hands_the_selection_over_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        std::fs::create_dir(&work).expect("work dir");
        let out = dir.path().join("argv");

        let mut surface = Services::new(
            dir.path(),
            HelperCommand {
                program: recorder(dir.path()),
                extra_args: vec![],
            },
        );
        surface.apply(&[target("d_1", "PC A")]).expect("apply");
        let script = plist_value(
            &dir.path()
                .join("UniversalLink-d_1.workflow")
                .join("Contents")
                .join("Resources")
                .join("document.wflow"),
            "actions.0.action.ActionParameters.COMMAND_STRING",
        );

        let selection = [
            "/tmp/plain.txt",
            "/tmp/two words.txt",
            "/tmp/$(touch pwned).txt",
            "/tmp/`touch pwned2`.txt",
            "/tmp/it's here.txt",
            "/tmp/line\nbreak.txt",
            "-r",
            "/tmp/100%.txt",
        ];
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            // What the engine passes as `$0`.
            .arg("-")
            .args(selection)
            .current_dir(&work)
            .env("OUT", &out)
            .status()
            .expect("run the script");
        assert!(status.success(), "the entry failed: {status:?}");

        let mut expected = vec!["--send".to_string(), "d_1".into(), "--".into()];
        expected.extend(selection.iter().map(|s| (*s).to_string()));
        assert_eq!(recorded(&out), expected);
        assert!(!work.join("pwned").exists(), "a file name was executed");
        assert!(!work.join("pwned2").exists(), "a file name was executed");
    }

    // -----------------------------------------------------------------------
    // The surface against a real directory.
    // -----------------------------------------------------------------------

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn applying_writes_one_bundle_per_target_in_the_shape_the_system_expects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Services::new(dir.path(), helper());
        assert_eq!(surface.dir(), dir.path());

        surface
            .apply(&[target("d_1", "PC A"), target("d_2", "PC B")])
            .expect("apply");

        assert_eq!(
            listing(dir.path()),
            ["UniversalLink-d_1.workflow", "UniversalLink-d_2.workflow"]
        );
        for name in ["UniversalLink-d_1.workflow", "UniversalLink-d_2.workflow"] {
            let contents = dir.path().join(name).join("Contents");
            assert!(contents.join("Info.plist").is_file());
            assert!(
                contents.join("Resources").join("document.wflow").is_file(),
                "the document belongs under Resources"
            );
        }
    }

    /// The absolute contract: after `apply`, the directory holds exactly the targets.
    #[test]
    fn bundles_that_are_no_longer_targets_are_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Services::new(dir.path(), helper());

        surface
            .apply(&[target("d_1", "PC A"), target("d_2", "PC B")])
            .expect("apply");
        surface.apply(&[target("d_1", "Bureau")]).expect("reapply");

        assert_eq!(listing(dir.path()), ["UniversalLink-d_1.workflow"]);
    }

    /// No manager, no entry — but the services directory itself is the system's, not
    /// ours, so it stays.
    #[test]
    fn an_empty_list_removes_every_bundle_and_keeps_the_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Services::new(dir.path(), helper());

        surface.apply(&[target("d_1", "PC A")]).expect("apply");
        surface.apply(&[]).expect("apply empty");
        assert_eq!(listing(dir.path()), Vec::<String>::new());
        assert!(dir.path().is_dir(), "the services directory was removed");
        // And the startup render, which always applies an empty list, must not fail
        // on a directory that was never created.
        let mut fresh = Services::new(&dir.path().join("nope"), helper());
        fresh
            .apply(&[])
            .expect("apply empty on a missing directory");
    }

    /// Pruning enumerates `~/Library/Services`, where every application's services
    /// live. Anything we did not write is never ours to remove.
    #[test]
    fn a_bundle_someone_else_left_here_is_never_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Services::new(dir.path(), helper());
        surface.apply(&[target("d_1", "PC A")]).expect("apply");

        let theirs = dir.path().join("Their Own.workflow");
        write_if_changed(
            &theirs.join("Contents").join("Info.plist"),
            "<plist><dict/></plist>",
        )
        .expect("write");
        let loose = dir.path().join("A Note.txt");
        std::fs::write(&loose, "not a bundle").expect("write");
        // And something shaped like nothing we write, but carrying our marker: only
        // what looks like one of our bundles is ours to delete.
        let odd = dir.path().join("UniversalLink-d_1.something-else");
        write_if_changed(
            &odd.join("Contents").join("Info.plist"),
            &info_plist("org.universallink.menu.d_1", "Send to Odd"),
        )
        .expect("write");

        surface.apply(&[]).expect("apply empty");
        assert!(theirs.is_dir(), "someone else's service was deleted");
        assert!(loose.is_file(), "an unrelated file was deleted");
        assert!(
            odd.is_dir(),
            "something that is not one of our bundles was deleted"
        );
        assert!(!dir.path().join("UniversalLink-d_1.workflow").exists());
    }

    /// A bundle left by a previous run — a device that is no longer online, a name
    /// from an older version — is ours and goes.
    #[test]
    fn a_bundle_from_a_previous_run_is_swept_at_startup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ghost = dir.path().join("UniversalLink-d_ghost.workflow");
        write_if_changed(
            &ghost.join("Contents").join("Info.plist"),
            &info_plist("org.universallink.menu.d_ghost", "Send to Ghost"),
        )
        .expect("write");

        let mut surface = Services::new(dir.path(), helper());
        // Exactly what the applier does first.
        surface.apply(&[]).expect("startup render");
        assert!(!ghost.exists(), "a stale bundle survived");
    }

    /// The write order, seen from its consequence: with the document unwritable, the
    /// plist that would register the service must not exist. In the other order the
    /// system would show an entry whose workflow is missing.
    #[test]
    fn the_plist_that_registers_the_service_is_written_last() {
        let dir = tempfile::tempdir().expect("tempdir");
        let contents = dir
            .path()
            .join("UniversalLink-d_1.workflow")
            .join("Contents");
        std::fs::create_dir_all(&contents).expect("mkdir");
        // A file where the document's directory has to go: the document cannot be
        // written, whatever the rest does.
        std::fs::write(contents.join("Resources"), "in the way").expect("write");

        let mut surface = Services::new(dir.path(), helper());
        surface
            .apply(&[target("d_1", "PC A")])
            .expect_err("the document could not be written");
        assert!(
            !contents.join("Info.plist").exists(),
            "the service was registered without its workflow"
        );
    }

    /// Re-applying the same list must not touch the tree: the system re-reads a
    /// bundle that changes, and the orchestrator re-applies the current list at
    /// startup and after any failure.
    #[test]
    fn re_applying_the_same_list_rewrites_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut surface = Services::new(dir.path(), helper());
        surface.apply(&[target("d_1", "PC A")]).expect("apply");

        let info = dir
            .path()
            .join("UniversalLink-d_1.workflow")
            .join("Contents")
            .join("Info.plist");
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&info)
            .expect("open")
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("set mtime");

        surface.apply(&[target("d_1", "PC A")]).expect("reapply");
        assert_eq!(
            std::fs::metadata(&info).expect("meta").modified().ok(),
            Some(old),
            "an unchanged bundle was rewritten"
        );
    }
}
