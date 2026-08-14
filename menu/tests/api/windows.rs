// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Windows surfaces, end to end: the artifacts the manager writes, and the
//! command line Explorer will actually run from one of them.
//!
//! Same claim as the Linux file, and the same reason it needs a live Core: that the
//! generated artifact is a working command line, and that it names the device its
//! label says. What differs is the oracle. There is no spec to write a second reader
//! from here — the command line is read back out of the real registry and split by
//! `CommandLineToArgvW`, which is not a second implementation of the quoting rules
//! but the one every program the shell starts is parsed by. The selection then
//! replaces the `%1` the shell would replace, and the REAL courier binary runs.
//!
//! Neither test needs Explorer, so both are part of the automated suite. What does
//! need it — whether the entries are SHOWN, and what the shell hands over for a
//! multiple selection — is in `tests/windows.rs`, `#[ignore]`d.

use std::path::{Path, PathBuf};

use onedevice_menu::os::windows::registry::Key;
use onedevice_menu::os::windows::{Cascade, SendTo};
use onedevice_menu::{HelperCommand, MenuSurface, Outcome};

use crate::support::*;

/// A registry root and a `SendTo` folder of this test's own, so nothing here ever
/// touches the developer's real shortcut menu.
struct Desktop {
    classes: String,
    send_to: tempfile::TempDir,
}

impl Desktop {
    fn new(tag: &str) -> Desktop {
        Desktop {
            classes: format!(
                r"Software\1Device-menu-api\{tag}-{}\Classes",
                std::process::id()
            ),
            send_to: tempfile::tempdir().expect("tempdir"),
        }
    }

    /// Where the cascade for a file selection lives.
    fn cascade(&self) -> String {
        format!(r"{}\*\shell\1Device", self.classes)
    }

    /// The real surfaces, whose entries start the real courier binary and point it
    /// at `channel`. The folders cascade is left out: it is the same code with
    /// another class, and its own keys are unit-tested.
    fn surfaces(&self, channel: &Path) -> Vec<Box<dyn MenuSurface>> {
        let helper = HelperCommand {
            program: PathBuf::from(env!("CARGO_BIN_EXE_1device-menu")),
            extra_args: vec!["--channel".into(), channel.to_string_lossy().into_owned()],
        };
        vec![
            Box::new(Cascade::files(&self.classes, helper.clone())),
            Box::new(SendTo::new(self.send_to.path(), helper)),
        ]
    }

    /// Polls until both surfaces show exactly these labels. The manager debounces,
    /// so the interesting assertion is always "it settles on this".
    async fn await_entries(&self, expected: &[&str]) {
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            let menu = self.cascade_labels();
            let send_to = self.send_to_labels();
            if menu == expected && send_to == expected {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the menu shows {menu:?}, Send to {send_to:?}, expected {expected:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// What the submenu displays: the `MUIVerb` of each child verb.
    fn cascade_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .verbs()
            .into_iter()
            .filter_map(|verb| {
                Key::open(&format!(r"{}\shell\{verb}", self.cascade()))
                    .expect("open")?
                    .string("MUIVerb")
            })
            .collect();
        labels.sort();
        labels
    }

    /// The child verb keys, in the order the registry hands them back — which is the
    /// order the submenu is shown in.
    fn verbs(&self) -> Vec<String> {
        match Key::open(&format!(r"{}\shell", self.cascade())).expect("open") {
            Some(key) => key.subkeys().expect("subkeys"),
            None => Vec::new(),
        }
    }

    /// And what "Send to" displays: each shortcut's name, without the extension the
    /// shell hides or the suffix that names us.
    fn send_to_labels(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.send_to.path()) else {
            return Vec::new();
        };
        let mut labels: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter_map(|name| name.strip_suffix(" (1Device).lnk").map(str::to_string))
            .collect();
        labels.sort();
        labels
    }

    /// The argv Explorer will run for `device_id`, from the command line the cascade
    /// really wrote: split by the shell's own parser, with the selection put where
    /// the shell puts it.
    fn cascade_argv(&self, device_id: &str, paths: &[PathBuf]) -> Vec<String> {
        let mut found = Vec::new();
        for verb in self.verbs() {
            let command = Key::open(&format!(r"{}\shell\{verb}\command", self.cascade()))
                .expect("open")
                .unwrap_or_else(|| panic!("{verb} has no command key"))
                .string("")
                .unwrap_or_else(|| panic!("{verb} has no command"));
            let argv = parse_command_line(&command);
            if !argv.iter().any(|arg| arg == device_id) {
                continue;
            }
            // The shell replaces `"%1"` with the selection. Done on the argv rather
            // than on the string, which is the same thing: the quotes we wrote
            // around the placeholder are what keep a path with a space one argument.
            found.push(
                argv.into_iter()
                    .flat_map(|arg| {
                        if arg == "%1" {
                            paths
                                .iter()
                                .map(|p| p.to_string_lossy().into_owned())
                                .collect()
                        } else {
                            vec![arg]
                        }
                    })
                    .collect::<Vec<String>>(),
            );
        }
        assert_eq!(
            found.len(),
            1,
            "{device_id} should be named by exactly one entry, found {found:?}"
        );
        found.pop().expect("checked just above")
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // The registry root is not a temporary directory: it has to be swept, or
        // every run of this suite would leave one behind.
        let (parent, name) = self
            .classes
            .rsplit_once('\\')
            .expect("the root has a parent");
        if let Ok(Some(key)) = Key::open(parent) {
            let _ = key.delete_subtree(name);
        }
        if let Ok(Some(key)) = Key::open(r"Software\1Device-menu-api") {
            let _ = key.delete_subtree(
                parent
                    .rsplit_once('\\')
                    .map(|(_, tag)| tag)
                    .unwrap_or(parent),
            );
        }
    }
}

/// Splits a command line with the parser Windows itself uses.
fn parse_command_line(line: &str) -> Vec<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    let mut count = 0i32;
    // SAFETY: a NUL-terminated command line and a valid out pointer; the returned
    // array is freed below, as the API requires.
    unsafe {
        let argv = CommandLineToArgvW(wide.as_ptr(), &mut count);
        assert!(!argv.is_null(), "CommandLineToArgvW refused: {line}");
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let arg = *argv.add(i);
            let mut end = arg;
            while *end != 0 {
                end = end.add(1);
            }
            out.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                arg,
                end.offset_from(arg) as usize,
            )));
        }
        LocalFree(argv as *mut std::ffi::c_void);
        out
    }
}

async fn run(argv: &[String]) -> String {
    let output = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
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

/// The whole chain the way Explorer triggers it: the command line out of the real
/// registry, the selection where the shell puts it, the real courier binary, and the
/// Core's own account of what it was asked to send.
///
/// Two peers are online and the SECOND is clicked: sending to "the first target"
/// instead of the clicked one is a mistake no assertion on a reply could catch, and
/// it would silently deliver the user's files to the wrong machine.
#[tokio::test]
async fn a_cascade_entry_sends_the_selection_to_the_device_it_names() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let mut watcher = TransferWatcher::connect(&core).await;
    let desktop = Desktop::new("click");
    let manager = Manager::start_with(&core, |channel| desktop.surfaces(channel)).await;

    let first = server.attested_peer(&code, "PC-A", "windows").await;
    let second = server.attested_peer(&code, "PC-B", "windows").await;
    desktop.await_entries(&["PC-A", "PC-B"]).await;

    // A name with a space, because that is what the quoting is for.
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = vec![
        a_file(dir.path(), "a note.txt"),
        a_file(dir.path(), "second.txt"),
    ];

    let argv = desktop.cascade_argv(&second.device_id, &paths);
    let transfer_id = run(&argv).await;
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

/// No manager, no entry. On Windows this is the ONLY cleanup opportunity there is —
/// the supervisor sends no signal, so standard input closing is the whole graceful
/// stop — and it has to reach both surfaces.
#[tokio::test]
async fn the_entries_go_when_the_manager_stops() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let desktop = Desktop::new("stop");
    let manager = Manager::start_with(&core, |channel| desktop.surfaces(channel)).await;

    let _peer = server.attested_peer(&code, "PC-B", "windows").await;
    desktop.await_entries(&["PC-B"]).await;

    assert_eq!(manager.stop().await, Outcome::StdinClosed);
    assert!(
        Key::open(&desktop.cascade()).expect("open").is_none(),
        "the cascade outlived the manager"
    );
    assert_eq!(
        desktop.send_to_labels(),
        Vec::<String>::new(),
        "a Send to entry outlived the manager"
    );
}
