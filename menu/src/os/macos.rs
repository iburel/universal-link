// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The macOS family-A surface: one Automator workflow per device in
//! `~/Library/Services`.
//!
//! # What the artifact is
//!
//! A `.workflow` bundle — a directory holding `Contents/Info.plist` and
//! `Contents/Resources/document.wflow` (the document sits under `Resources`, not
//! next to `Info.plist`). The Info.plist declares an `NSServices` provider whose
//! `NSMessage` is `runWorkflowAsService`, which is how the system's own workflow
//! runner is told to execute the document; the document holds a single "Run Shell
//! Script" action whose script starts our courier with the selection.
//!
//! So the OS keeps a command line on disk and starts a fresh process from it, like
//! every other surface here — with Apple's runner as the thing that reads it.
//!
//! # Every fact below was read off a real macOS, or measured on one
//!
//! There is no specification for any of this. The shapes come from Apple's own
//! shipping workflows on a real macOS 26.5.2 (`/System/Library/Services/Set Desktop
//! Picture.workflow`, `Encode Selected Video Files.workflow`) and from the action
//! bundle the document points at (`/System/Library/Automator/Run Shell
//! Script.action`, whose `AMDefaultParameters` names every parameter). What could
//! not be read was measured on that machine:
//!
//! - **`inputMethod = 1` means "as arguments".** Measured: with `1`, the script got
//!   the paths in `"$@"` and nothing on standard input. `$0` is `-`.
//! - **One process for the whole selection.** Measured by feeding a three-item list
//!   through the real engine (a "Get Specified Finder Items" action ahead of the
//!   script): one process, `argc=3`, the folder included. The click coalescer
//!   ([`crate::clicks`]) covers the other outcome anyway — Automator is free to
//!   chunk a long list — so a burst can only cost extra transfers, never a lost
//!   file.
//! - **The menu label is the plist string, not the bundle's name.** Measured: a
//!   lookup by `NSMenuItem.default` resolves the service (the system answered with
//!   the pasteboard types it expects), a lookup by `CFBundleName` resolves nothing.
//!   That is why the bundle can be named after the device *id* and this surface has
//!   no file-name problem at all — the one thing that makes Nautilus hard.
//! - **`NSSendFileTypes = ["public.item"]` covers files AND folders.** Measured: a
//!   selection of only a folder matches. Hence ONE surface here, where Windows needs
//!   a cascade for files and another for folders.
//! - **The system follows the directory on its own, with a lag.** Measured on an idle
//!   machine: an appearance shows up after ~1-2 s, a bundle modified in place after
//!   ~7 s (old label gone with it), a bundle deleted after ~7 s. An earlier
//!   measurement that stopped at 6 s made the last one look permanent; it is not.
//!   [`notify_services_changed`] is therefore about LATENCY, not correctness: it makes
//!   all three immediate, and it does not depend on how loaded the machine is. Worth
//!   the one call — in those seconds a menu shows an entry whose click does nothing —
//!   but nothing here is load-bearing on it, and no test can see it (see the mutation
//!   note in `tests/macos.rs`).
//! - **Two bundles carrying the SAME label both register**, and the label is also
//!   what a lookup resolves — so nothing disambiguates two devices with one name for
//!   us. See [`services`] for the ladder that does.
//!
//! # Deliberate choices
//!
//! **A Services-menu workflow, not an Automator "Quick Action" document.** Modern
//! Finder shows workflow services under "Quick Actions" itself (its binary carries
//! `NSLegacyServiceQuickAction`), and the services-menu form is the one that ships
//! on the machine, so it is the one whose every key could be read rather than
//! guessed. The document is trimmed to what the engine demonstrably needs — measured
//! by removing keys until it stopped mattering — with the service metadata Apple's
//! own workflows carry kept as they carry it. What is NOT written is
//! `AMApplicationBuild`/`AMApplicationVersion`: they say which Automator wrote the
//! file, the engine does not need them, and we are not Automator. If a future macOS
//! turns out to want them, they are two lines.
//!
//! **`~/Library/Services` is never removed**, even when we leave it empty: it is the
//! system's directory for every application's services, not ours. Contrast
//! [`crate::os::linux`], which removes the subdirectory it owns.
//!
//! **No `NSRequiredContext`.** Apple ships workflows both with one (`Encode Selected
//! Video Files`, gated on Finder) and without (`Set Desktop Picture`), and gating on
//! Finder would match this chantier's scope exactly — the file manager's contextual
//! menu. It is left out all the same, because the shape that is *known* to be shown
//! by a real Finder for a hand-written workflow is the one without it: an earlier
//! spike of this route, validated end to end on this same machine (macOS 26.5.1,
//! 2026-07-01, `~/contextual-menu/poc/macos-services`), carries no such key and its
//! entry appeared and ran. A registered-but-invisible entry is the one failure a user
//! cannot debug and no automated test here can see, so this surface stays on the
//! shape with evidence behind it. Gating is one key, the day it matters.
//!
//! # Where the entry shows, and the one thing a user may have to do
//!
//! In the **Services** submenu: automatic, as soon as the service registers. Finder
//! also has an inline **Quick Actions** area in the same menu, and an entry appearing
//! *there* may need a checkbox in System Settings (→ General → Login Items &
//! Extensions → Finder). No code can tick it — an extension's activation is the
//! user's to grant. So the promise this surface makes is the Services submenu; the
//! Quick Actions row is a bonus when it is enabled.
//!
//! # The escaping obligation, fourth answer: two of them, nested
//!
//! The label is a plist string and the command line is a shell script *inside* that
//! string, so both of this component's earlier answers apply at once and in order:
//! the script is built with brick 2's single-quoting, then the whole thing is XML
//! escaped. A device named `<& '$(rm -rf ~)'` must come out as that text in a menu,
//! and our own install path must survive both layers.
//!
//! One thing XML cannot do is carry a C0 control character — not even as a numeric
//! reference (XML 1.0's `Char` production excludes them, along with U+FFFE and
//! U+FFFF). A label holding one is cleaned, because a control character has no place
//! in a menu either. Our own path is NOT cleaned: mangling it would write an entry
//! naming a program that does not exist, so [`services`] refuses instead.

mod services;

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

pub use services::Services;

use crate::surface::{HelperCommand, MenuSurface};

/// Marks a bundle as ours, as a key of its `Info.plist`.
///
/// Load-bearing: pruning enumerates `~/Library/Services` — a directory shared with
/// every other application's services — and deletes from it. Anything without this
/// is left strictly alone.
pub(crate) const MARKER: &str = "1device-menu:generated";
/// The `Info.plist` key it is the value of. Invisible in menus (nothing reads an
/// unknown key) and it travels with the bundle.
pub(crate) const MARKER_KEY: &str = "1DeviceGenerated";

/// Name of the temporary file the atomic writes go through, hidden so that a crash
/// between the write and the rename cannot leave a plist half-parsed.
const TMP_NAME: &str = ".1device-menu.tmp";

/// The one macOS surface, rooted at `services` (`~/Library/Services` in
/// production).
pub fn surfaces(services: &Path, helper: HelperCommand) -> Vec<Box<dyn MenuSurface>> {
    vec![Box::new(Services::new(services, helper))]
}

/// `~/Library/Services`, the per-user half of the two directories the system reads
/// services from (the other is `/Library/Services`, which needs root).
pub fn services_dir() -> Option<PathBuf> {
    services_dir_from(&|key| std::env::var_os(key))
}

/// Same, with the environment injected — `std::env::set_var` is unsafe and
/// process-wide in edition 2024, so the rule is tested this way rather than by
/// mutating the test process.
fn services_dir_from(get: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    // A relative `HOME` would scatter bundles wherever the Core happened to be
    // started from, and the system would read none of them.
    let home = get("HOME").map(PathBuf::from).filter(|h| h.is_absolute())?;
    Some(home.join("Library").join("Services"))
}

/// Writes `content` at `path` unless the file is already exactly that, creating the
/// parent directories if needed. Reports whether it wrote.
///
/// Atomic: a temporary file in the same directory, then `rename(2)`. The system
/// watches this tree and re-reads a bundle when it changes, so it must never catch
/// half an `Info.plist` — an invalid one is a device with no entry at all.
///
/// Skipping an identical write is not an optimization either: rewriting the same
/// bytes would make the system re-register services that did not change, and the
/// orchestrator re-applies the current list at startup and after any failure.
pub(crate) fn write_if_changed(path: &Path, content: &str) -> io::Result<bool> {
    if std::fs::read(path).is_ok_and(|bytes| bytes == content.as_bytes()) {
        return Ok(false);
    }
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("no directory to write {} into", path.display()),
        )
    })?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(TMP_NAME);
    let written = std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map(|()| true)
}

/// Removes a whole bundle. Absent is success: `apply(&[])` runs at every startup,
/// when there is usually nothing left to remove.
pub(crate) fn remove_bundle(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Whether `bundle` is one of ours, i.e. its `Info.plist` carries [`MARKER`].
///
/// The read is bounded: this runs on whatever another application has put in
/// `~/Library/Services`, and the marker is one short key near the top of a file we
/// wrote ourselves.
pub(crate) fn is_ours(bundle: &Path) -> bool {
    use std::io::Read;

    let Ok(file) = std::fs::File::open(bundle.join("Contents").join("Info.plist")) else {
        return false;
    };
    let mut head = Vec::new();
    if file.take(64 * 1024).read_to_end(&mut head).is_err() {
        return false;
    }
    head.windows(MARKER.len()).any(|w| w == MARKER.as_bytes())
}

/// Escapes what an XML text node cannot hold literally. The plists are written as
/// text, and a device name — or an install path — is arbitrary Unicode.
pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Whether `c` can appear in an XML 1.0 document at all. Everything else is
/// unrepresentable: escaping does not help, because the `Char` production excludes
/// it however it is spelled.
pub(crate) fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && c != '\u{FFFE}' && c != '\u{FFFF}')
}

/// Tells the system that the set of services on disk changed.
///
/// The system would get there by itself — measured at about seven seconds for a
/// removal or a rename on an idle machine (see the module header). This makes it
/// immediate, which is worth doing because during that window a menu offers an entry
/// whose click does nothing, or names a device by its old name. It is not what makes
/// the surface correct.
///
/// `NSUpdateDynamicServices` is AppKit's documented way to say exactly this, and it
/// is a bare C function: no Objective-C messaging, no new dependency, and — unlike
/// spawning `pbs -flush` — no process. It is called from the blocking thread the
/// surfaces are applied on; it talks to the pasteboard server and touches no UI.
///
/// Linking AppKit costs the COURIER too, since that is the same binary and it runs
/// once per click. Measured on the test Mac: about 23 ms for a whole courier run,
/// framework load included — AppKit lives in the dyld shared cache. Not worth
/// resolving the symbol by hand at call time to avoid.
pub(crate) fn notify_services_changed() {
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {
        fn NSUpdateDynamicServices();
    }
    // SAFETY: a C function taking and returning nothing, from a framework that is
    // always present on macOS.
    unsafe { NSUpdateDynamicServices() };
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
    fn the_services_directory_hangs_off_an_absolute_home() {
        assert_eq!(
            services_dir_from(&env_of(&[("HOME", "/Users/i")])),
            Some(PathBuf::from("/Users/i/Library/Services"))
        );
        assert_eq!(services_dir_from(&env_of(&[])), None);
        assert_eq!(services_dir_from(&env_of(&[("HOME", "Users/i")])), None);
        assert_eq!(services_dir_from(&env_of(&[("HOME", "")])), None);
    }

    #[test]
    fn an_identical_write_is_skipped_and_reported_as_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("Contents").join("Info.plist");

        assert!(write_if_changed(&path, "one").expect("first write"));
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "one");

        // Rewriting the same bytes would make the system re-register a service that
        // did not change.
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("set mtime");
        assert!(!write_if_changed(&path, "one").expect("second write"));
        assert_eq!(
            std::fs::metadata(&path).expect("meta").modified().ok(),
            Some(old),
            "an unchanged plist was rewritten"
        );

        assert!(write_if_changed(&path, "two").expect("third write"));
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "two");
    }

    #[test]
    fn writing_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_if_changed(&dir.path().join("Info.plist"), "one").expect("write");
        assert!(!dir.path().join(TMP_NAME).exists());
    }

    #[test]
    fn removing_a_bundle_that_is_not_there_is_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        remove_bundle(&dir.path().join("Absent.workflow")).expect("absent is fine");
    }

    #[test]
    fn only_a_bundle_whose_plist_carries_the_marker_is_ours() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ours = dir.path().join("Ours.workflow");
        write_if_changed(
            &ours.join("Contents").join("Info.plist"),
            &format!("<key>{MARKER_KEY}</key><string>{MARKER}</string>"),
        )
        .expect("write");
        let theirs = dir.path().join("Theirs.workflow");
        write_if_changed(
            &theirs.join("Contents").join("Info.plist"),
            "<key>CFBundleName</key><string>Theirs</string>",
        )
        .expect("write");

        assert!(is_ours(&ours));
        assert!(!is_ours(&theirs));
        assert!(!is_ours(&dir.path().join("Absent.workflow")));
        // A bundle with no Info.plist at all is not ours either — and asking must
        // not fail.
        std::fs::create_dir(dir.path().join("Empty.workflow")).expect("mkdir");
        assert!(!is_ours(&dir.path().join("Empty.workflow")));
    }

    /// A big file another application left here must not be read whole just to
    /// answer "is this ours" — nor claimed.
    #[test]
    fn a_large_foreign_plist_is_not_ours() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("Big.workflow");
        let mut content = "x".repeat(128 * 1024);
        content.push_str(MARKER);
        write_if_changed(&bundle.join("Contents").join("Info.plist"), &content).expect("write");
        assert!(!is_ours(&bundle));
    }

    #[test]
    fn xml_escaping_covers_what_a_text_node_cannot_hold() {
        assert_eq!(xml_escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        // A quote needs nothing in a text node, and the script is full of them.
        assert_eq!(xml_escape(r#"'a' "b""#), r#"'a' "b""#);
        // No double escaping.
        assert_eq!(xml_escape("&amp;"), "&amp;amp;");
    }

    #[test]
    fn the_characters_xml_cannot_carry_are_the_control_ones() {
        for c in [
            '\t',
            '\n',
            '\r',
            ' ',
            'a',
            'é',
            '🇫',
            '\u{FDD0}',
            '\u{10FFFF}',
        ] {
            assert!(is_xml_char(c), "{c:?} is legal in XML 1.0");
        }
        for c in ['\0', '\u{1}', '\u{8}', '\u{B}', '\u{C}', '\u{1F}'] {
            assert!(!is_xml_char(c), "{c:?} is not legal in XML 1.0");
        }
        assert!(!is_xml_char('\u{FFFE}'));
        assert!(!is_xml_char('\u{FFFF}'));
    }
}
