// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Linux surfaces, end to end: the artifacts the manager writes, and the
//! command lines Dolphin and Nautilus will actually run from them.
//!
//! This is the only place the whole chain is exercised the way a user triggers it
//! — surface → command line → a REAL courier process → the manager's channel →
//! `files.send`. Everything else about a click is unit-tested; what needs a live
//! Core is the claim that the generated artifact is a working command line, and
//! that it names the device its label says.
//!
//! For Dolphin that means parsing the `Exec=` value here, with a reader written
//! from the Desktop Entry spec rather than from the writer: two implementations of
//! the same spec agreeing is evidence, whereas reusing the writer's own escaping
//! would only prove it is self-consistent. For Nautilus there is nothing to parse
//! — `/bin/sh` is the second implementation, and the test runs the entry exactly as
//! Nautilus does (the browsed folder as the working directory, bare names as
//! arguments).

use std::path::{Path, PathBuf};
use tokio::process::Command;

use onedevice_menu::os::linux;
use onedevice_menu::{HelperCommand, MenuSurface, Outcome, Target};

use crate::support::*;

/// A temporary `$XDG_DATA_HOME`, so a test never touches the developer's own
/// desktop menus.
struct Desktop {
    dir: tempfile::TempDir,
}

impl Desktop {
    fn new() -> Desktop {
        Desktop {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The KDE artifact.
    fn servicemenu(&self) -> PathBuf {
        self.path()
            .join("kio")
            .join("servicemenus")
            .join("1device-send.desktop")
    }

    /// The Nautilus submenu directory.
    fn scripts(&self) -> PathBuf {
        self.path().join("nautilus").join("scripts").join("1Device")
    }

    /// The real surfaces, whose entries start the real courier binary and point it
    /// at `channel`.
    fn surfaces(&self, channel: &Path) -> Vec<Box<dyn MenuSurface>> {
        linux::surfaces(
            self.path(),
            HelperCommand {
                program: PathBuf::from(env!("CARGO_BIN_EXE_1device-menu")),
                extra_args: vec!["--channel".into(), channel.to_string_lossy().into_owned()],
            },
        )
    }

    /// Polls until the artifacts show exactly these labels. The manager debounces,
    /// so the interesting assertion is always "it settles on this".
    async fn await_entries(&self, expected: &[&str]) {
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            let scripts = self.script_names();
            let names = self.desktop_names();
            if scripts == expected && names == expected {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("Dolphin shows {names:?}, Nautilus {scripts:?}, expected {expected:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// The labels of the Nautilus entries: its file names, which is what it shows.
    fn script_names(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.scripts()) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The labels of the Dolphin entries, in the order the file lists them.
    fn desktop_names(&self) -> Vec<String> {
        let Ok(content) = std::fs::read_to_string(self.servicemenu()) else {
            return Vec::new();
        };
        let mut names: Vec<String> = groups(&content)
            .into_iter()
            .filter(|(group, _)| group.starts_with("Desktop Action "))
            .filter_map(|(_, keys)| value_of(&keys, "Name").map(unescape))
            .collect();
        names.sort();
        names
    }
}

// ---------------------------------------------------------------------------
// A reader for what we wrote, from the Desktop Entry spec.
// ---------------------------------------------------------------------------

/// The `[group]`s of a desktop file with their raw key/value pairs, in order.
/// Values are NOT unescaped here: that is per value, and per type.
fn groups(content: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push((name.to_string(), Vec::new()));
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("neither a group nor a key: {line:?}"));
        out.last_mut()
            .expect("a key before any group")
            .1
            .push((key.trim().to_string(), value.trim().to_string()));
    }
    out
}

fn value_of(keys: &[(String, String)], key: &str) -> Option<String> {
    let found: Vec<&String> = keys
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v)
        .collect();
    assert!(found.len() <= 1, "{key} appears {} times", found.len());
    found.first().map(|v| (*v).clone())
}

/// The spec's escape sequences for a value of type string. An unknown one is left
/// as it stands, which is what KConfig does with it.
fn unescape(value: String) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The spec's `Exec` grammar: whitespace separates arguments unless they are
/// inside double quotes, where a backslash escapes the character after it. `%%` is
/// a literal percent; any other `%x` is a field code, kept verbatim for the caller
/// to substitute.
fn tokenize(exec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if quoted => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            ' ' | '\t' if !quoted => {
                if started {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '%' => {
                match chars.next() {
                    Some('%') => current.push('%'),
                    Some(code) => {
                        current.push('%');
                        current.push(code);
                    }
                    None => current.push('%'),
                }
                started = true;
            }
            _ => {
                current.push(c);
                started = true;
            }
        }
    }
    assert!(!quoted, "unbalanced quotes in {exec:?}");
    if started {
        out.push(current);
    }
    out
}

/// The label and command line of the Dolphin entry that sends to `device_id`,
/// with `%F` replaced by `paths` — which is what KIO does with the selection.
fn dolphin_entry(content: &str, device_id: &str, paths: &[PathBuf]) -> (String, Vec<String>) {
    let mut found = Vec::new();
    for (group, keys) in groups(content) {
        if !group.starts_with("Desktop Action ") {
            continue;
        }
        let exec = value_of(&keys, "Exec").expect("an action without an Exec");
        let argv = tokenize(&unescape(exec));
        if !argv.iter().any(|arg| arg == device_id) {
            continue;
        }
        let name = value_of(&keys, "Name").map(unescape).expect("Name");
        let argv = argv
            .into_iter()
            .flat_map(|arg| {
                if arg == "%F" {
                    paths
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect()
                } else {
                    vec![arg]
                }
            })
            .collect();
        found.push((name, argv));
    }
    assert_eq!(
        found.len(),
        1,
        "{device_id} should be named by exactly one entry, found {found:?}"
    );
    found.remove(0)
}

/// Runs a command line and returns its standard output, asserting it succeeded.
///
/// Async on purpose: the courier is a real process talking to a manager that runs
/// on THIS runtime, so blocking the thread while waiting for it would deadlock the
/// two — the manager would never get to answer.
async fn run(argv: &[String], cwd: &Path) -> String {
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .output()
        .await
        .unwrap_or_else(|e| panic!("cannot run {argv:?}: {e}"));
    assert!(
        output.status.success(),
        "{argv:?} failed: {} / {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn target(id: &str, name: &str) -> Target {
    Target {
        device_id: id.into(),
        name: name.into(),
        platform: "linux".into(),
    }
}

// ---------------------------------------------------------------------------
// The artifacts, without a Core.
// ---------------------------------------------------------------------------

/// The escaping, read back by something other than the writer. A device name is
/// user-controlled and lands in a line-based format whose `Exec=` the desktop
/// runs; our own path is out of our hands too (an install directory with a space
/// in it is ordinary).
#[test]
fn a_hostile_device_name_survives_a_round_trip_through_the_spec() {
    let desktop = Desktop::new();
    let program = PathBuf::from(r"/opt/My Apps/back\slash/1device-menu");
    let hostile = "PC\nExec=/bin/sh -c \"rm -rf ~\"\t100% \"quoted\" $HOME";
    let mut surfaces = linux::surfaces(
        desktop.path(),
        HelperCommand {
            program: program.clone(),
            extra_args: vec![],
        },
    );
    for surface in surfaces.iter_mut() {
        surface.apply(&[target("d_1", hostile)]).expect("apply");
    }

    let content = std::fs::read_to_string(desktop.servicemenu()).expect("read");
    let selection = [PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/c.txt")];
    let (name, argv) = dolphin_entry(&content, "d_1", &selection);

    // The label reaches KDE intact — the newline included, which is exactly what
    // must NOT have become a key of its own.
    assert_eq!(name, hostile);
    assert_eq!(
        argv,
        [
            program.to_string_lossy().into_owned(),
            "--send".into(),
            "d_1".into(),
            "--".into(),
            "/tmp/a b.txt".into(),
            "/tmp/c.txt".into(),
        ]
    );

    // And Nautilus, whose label IS a file name: the newline and the tab became
    // spaces, and the separators — which a file name cannot hold at all — became
    // dashes. One entry, visible, and nothing that could be a path.
    assert_eq!(
        desktop.script_names(),
        [r#"PC Exec=-bin-sh -c "rm -rf ~" 100% "quoted" $HOME"#]
    );
}

// ---------------------------------------------------------------------------
// The clicks, against a real Core.
// ---------------------------------------------------------------------------

/// A Dolphin click, played the way KIO plays it: the `Exec=` line, parsed and
/// with the selection substituted for `%F`, run as a fresh process. Two peers are
/// online and the SECOND is clicked, so an entry that sent to "the first target"
/// would fail here — and in production that delivers the user's files to the wrong
/// machine.
#[tokio::test]
async fn a_dolphin_entry_sends_the_selection_to_the_device_it_names() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let mut watcher = TransferWatcher::connect(&core).await;
    let desktop = Desktop::new();
    let manager = Manager::start_with(&core, |channel| desktop.surfaces(channel)).await;

    // Kept alive: dropping a peer takes it offline, and the point is that TWO are
    // on offer when one of them is clicked.
    let _first = server.attested_peer(&code, "PC-A", "linux").await;
    let second = server.attested_peer(&code, "PC-B", "linux").await;
    desktop.await_entries(&["PC-A", "PC-B"]).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let paths = vec![a_file(dir.path(), "notes.txt"), a_file(dir.path(), "b.txt")];

    let content = std::fs::read_to_string(desktop.servicemenu()).expect("read");
    let (name, argv) = dolphin_entry(&content, &second.device_id, &paths);
    assert_eq!(
        name, "PC-B",
        "the entry for {} is mislabelled",
        second.device_id
    );

    // The courier prints the transfer it obtained; the Core says what it took on.
    let transfer_id = run(&argv, dir.path()).await;
    let started = watcher.started().await;
    assert_eq!(started["transfer_id"].as_str(), Some(transfer_id.as_str()));
    assert_eq!(
        started["device_id"].as_str(),
        Some(second.device_id.as_str()),
        "the files went to the wrong device"
    );
    assert_eq!(
        TransferWatcher::manifest_names(&started),
        ["notes.txt", "b.txt"]
    );

    drop(manager);
}

/// A Nautilus click, played the way Nautilus plays it: the script run with the
/// browsed folder as the working directory and the selected names — not paths —
/// as arguments. The Core resolves a relative path against ITS own directory, so
/// this is also the test that the courier's cwd-joining is what it is for.
#[tokio::test]
async fn a_nautilus_entry_sends_the_selection_named_relative_to_the_browsed_folder() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let mut watcher = TransferWatcher::connect(&core).await;
    let desktop = Desktop::new();
    let manager = Manager::start_with(&core, |channel| desktop.surfaces(channel)).await;

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    desktop.await_entries(&["PC-B"]).await;

    // A folder that is NOT the Core's working directory, and a name that would be
    // a flag if the separator were missing.
    let browsed = tempfile::tempdir().expect("tempdir");
    a_file(browsed.path(), "notes.txt");
    a_file(browsed.path(), "-r");

    let entry = desktop.scripts().join("PC-B");
    let transfer_id = run(
        &[
            entry.to_string_lossy().into_owned(),
            "notes.txt".into(),
            "-r".into(),
        ],
        browsed.path(),
    )
    .await;

    let started = watcher.started().await;
    assert_eq!(started["transfer_id"].as_str(), Some(transfer_id.as_str()));
    assert_eq!(started["device_id"].as_str(), Some(peer.device_id.as_str()));
    assert_eq!(
        TransferWatcher::manifest_names(&started),
        ["notes.txt", "-r"]
    );

    drop(manager);
}

/// Fail-closed on the desktop: the entries go when the target does. A peer that
/// went offline is a `files.send` that would fail, and an entry that always fails
/// is worse than no entry.
#[tokio::test]
async fn the_entries_go_when_the_last_device_goes_offline() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let desktop = Desktop::new();
    let manager = Manager::start_with(&core, |channel| desktop.surfaces(channel)).await;

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    desktop.await_entries(&["PC-B"]).await;

    drop(peer);
    desktop.await_entries(&[]).await;
    assert!(!desktop.servicemenu().exists());
    assert!(!desktop.scripts().exists());

    drop(manager);
}

/// The other half of the rule: no manager, no entry. The peer is still online —
/// what goes away is us, and a click on a leftover entry would find no channel and
/// fail in silence.
#[tokio::test]
async fn the_entries_go_when_the_manager_stops() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let desktop = Desktop::new();
    let manager = Manager::start_with(&core, |channel| desktop.surfaces(channel)).await;

    let _peer = server.attested_peer(&code, "PC-B", "linux").await;
    desktop.await_entries(&["PC-B"]).await;

    assert_eq!(manager.stop().await, Outcome::StdinClosed);
    assert!(
        !desktop.servicemenu().exists(),
        "the KDE entry outlived the manager"
    );
    assert!(
        !desktop.scripts().exists(),
        "the Nautilus entries outlived the manager"
    );
}
