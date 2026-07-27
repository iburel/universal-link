// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The macOS surface, end to end: the bundles the manager writes, and the script the
//! system's workflow runner will actually execute from one of them.
//!
//! Same claim as the Linux and Windows files, and the same reason it needs a live
//! Core: that a generated artifact is a working command line, and that it sends to the
//! device its label names. What differs is the oracle. The script is not re-derived
//! here — it is read back out of the document with `plutil`, the plist parser macOS
//! itself uses, and handed to `/bin/sh` the way the workflow engine hands it over:
//! the selection as arguments, `$0` set to `-` (both measured on a real macOS, see
//! `universallink_menu::os::macos`). Then the REAL courier binary runs.
//!
//! Neither test needs Finder, so both are part of the automated suite. What does need
//! it — whether the entry is SHOWN in a real contextual menu — is in `tests/macos.rs`,
//! `#[ignore]`d, together with the checks that go through the real services registry.

use std::path::{Path, PathBuf};

use universallink_menu::os::macos::Services;
use universallink_menu::{HelperCommand, MenuSurface, Outcome};

use crate::support::*;

/// A services directory of this test's own, so nothing here ever touches the
/// developer's real contextual menu.
struct Desktop {
    services: tempfile::TempDir,
}

impl Desktop {
    fn new() -> Desktop {
        Desktop {
            services: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn dir(&self) -> &Path {
        self.services.path()
    }

    /// The real surface, whose bundles start the real courier binary and point it at
    /// `channel`.
    fn surfaces(&self, channel: &Path) -> Vec<Box<dyn MenuSurface>> {
        let helper = HelperCommand {
            program: PathBuf::from(env!("CARGO_BIN_EXE_universallink-menu")),
            extra_args: vec!["--channel".into(), channel.to_string_lossy().into_owned()],
        };
        vec![Box::new(Services::new(self.dir(), helper))]
    }

    /// Polls until the surface shows exactly these device names. The manager
    /// debounces, so the interesting assertion is always "it settles on this".
    async fn await_entries(&self, expected: &[&str]) {
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            let shown = self.names();
            if shown == expected {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the menu shows {shown:?}, expected {expected:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// The device names the menu shows — which also pins the label's shape, since
    /// that is what has to be stripped back off.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .bundles()
            .iter()
            .map(|bundle| {
                let label = plist_value(
                    &bundle.join("Contents").join("Info.plist"),
                    "NSServices.0.NSMenuItem.default",
                );
                label
                    .strip_prefix("Send to ")
                    .and_then(|l| l.strip_suffix(" (UniversalLink)"))
                    .unwrap_or_else(|| panic!("{label:?} is not one of our labels"))
                    .to_string()
            })
            .collect();
        names.sort();
        names
    }

    /// Every bundle in the directory, in no particular order.
    fn bundles(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.dir()) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "workflow"))
            .collect()
    }

    /// The script the runner would execute for `device_id`, read out of the document
    /// the surface really wrote.
    fn script_for(&self, device_id: &str) -> String {
        let needle = format!("'{device_id}'");
        let mut found: Vec<String> = self
            .bundles()
            .iter()
            .map(|bundle| {
                plist_value(
                    &bundle
                        .join("Contents")
                        .join("Resources")
                        .join("document.wflow"),
                    "actions.0.action.ActionParameters.COMMAND_STRING",
                )
            })
            .filter(|script| script.contains(&needle))
            .collect();
        assert_eq!(
            found.len(),
            1,
            "{device_id} should be named by exactly one entry, found {}",
            found.len()
        );
        found.pop().expect("checked just above")
    }
}

/// Reads one value out of a plist with the parser macOS itself uses.
fn plist_value(path: &Path, key: &str) -> String {
    let out = std::process::Command::new("/usr/bin/plutil")
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

/// Runs a script the way the workflow engine does. `tokio::process`, never
/// `std::process`: these tests run on a single-threaded runtime, and blocking it
/// would stop the in-process manager from ever answering the courier.
async fn run_script(script: &str, paths: &[PathBuf]) -> String {
    let mut command = tokio::process::Command::new("/bin/sh");
    command.arg("-c").arg(script).arg("-");
    for path in paths {
        command.arg(path);
    }
    let output = tokio::time::timeout(RESPONSE_TIMEOUT, command.output())
        .await
        .expect("the entry did not finish in time")
        .expect("the entry could not be started");
    assert!(
        output.status.success(),
        "the entry failed: {} / {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The whole chain the way Finder triggers it: the script out of the real document,
/// the selection where the engine puts it, the real courier binary, and the Core's own
/// account of what it was asked to send.
///
/// Two peers are online and the SECOND is clicked: sending to "the first target"
/// instead of the clicked one is a mistake no assertion on a reply could catch, and it
/// would silently deliver the user's files to the wrong machine.
#[tokio::test]
async fn a_service_entry_sends_the_selection_to_the_device_it_names() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let mut watcher = TransferWatcher::connect(&core).await;
    let desktop = Desktop::new();
    let manager = Manager::start_with(&core, |channel| desktop.surfaces(channel)).await;

    let first = server.attested_peer(&code, "PC-A", "macos").await;
    let second = server.attested_peer(&code, "PC-B", "macos").await;
    desktop.await_entries(&["PC-A", "PC-B"]).await;

    // A name with a space, because that is what the single-quoting is for.
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = vec![
        a_file(dir.path(), "a note.txt"),
        a_file(dir.path(), "second.txt"),
    ];

    let transfer_id = run_script(&desktop.script_for(&second.device_id), &paths).await;
    assert!(
        transfer_id.starts_with("t_"),
        "the courier printed {transfer_id:?}"
    );

    let started = watcher.started().await;
    assert_eq!(started["transfer_id"].as_str(), Some(transfer_id.as_str()));
    assert_eq!(
        started["device_id"].as_str(),
        Some(second.device_id.as_str()),
        "the files went to the wrong device"
    );
    let mut names = TransferWatcher::manifest_names(&started);
    names.sort();
    assert_eq!(names, ["a note.txt", "second.txt"]);

    let _ = first;
    let _ = manager;
}

/// No manager, no entry. Here that means the bundles are gone — and the services
/// directory, which belongs to every application that has a service, is still there.
#[tokio::test]
async fn the_entries_go_when_the_manager_stops() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let desktop = Desktop::new();
    let manager = Manager::start_with(&core, |channel| desktop.surfaces(channel)).await;

    let _peer = server.attested_peer(&code, "PC-B", "macos").await;
    desktop.await_entries(&["PC-B"]).await;

    assert_eq!(manager.stop().await, Outcome::StdinClosed);
    assert_eq!(
        desktop.bundles().len(),
        0,
        "a bundle outlived the manager: {:?}",
        desktop.bundles()
    );
    assert!(desktop.dir().is_dir(), "the services directory was removed");
}
