// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Live Windows-shell tests for the contextual-menu surfaces.
//!
//! These write to the REAL registry (`HKCU\Software\Classes`) and the REAL `SendTo`
//! folder, and they ask the REAL shell what it makes of them. Each one puts the
//! machine back as it found it.
//!
//! All are `#[ignore]`d: the workspace CI runs `cargo test` without `--ignored`, so
//! they are not part of the automated suite. They are run by hand on a real Windows
//! session with:
//!
//!     cargo test -p universallink-menu --test windows -- --ignored --test-threads=1
//!
//! Single-threaded, because they share one namespace: the user's own menus.
//!
//! # What is left to a human, and why
//!
//! One thing: **a right click on a multiple selection**. Invoking a verb on several
//! items at once is something only a real menu (or an `IContextMenu` walked with a
//! message pump) can do — `Shell.Application` invokes a verb on ONE item. So what
//! these tests prove is that the shell reads our command line and runs it correctly;
//! what `MultiSelectModel=Player` then does with five selected files is confirmed by
//! a human selecting five files. The manager coalesces either way (see
//! `universallink_menu::clicks`), so the only outcome that would matter is the shell
//! handing over a PART of the selection — which is exactly what such a gesture
//! shows.

#![cfg(target_os = "windows")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use universallink_menu::channel::{self, Request, Response};
use universallink_menu::os::windows::{Cascade, SendTo, verb_command_line};
use universallink_menu::{HelperCommand, MenuSurface, Target};

/// The production root: this is the point of these tests.
const CLASSES: &str = r"Software\Classes";
/// How long a click is given to reach the probe.
const CLICK_TIMEOUT: Duration = Duration::from_secs(20);

fn target() -> Target {
    Target {
        device_id: "d_livetest".into(),
        // Deliberately holding an ampersand and a space: what a menu does with them
        // is visible in the same gesture that checks the entry is there.
        name: "PC-Live & Co".into(),
        platform: "windows".into(),
    }
}

/// A channel name carrying a PERCENT SIGN.
///
/// Not decoration: the shell reads a verb's command line and substitutes its field
/// codes (`%1`, `%V`, …) before anything else sees it, and `%%` is the only escape
/// there is for a literal one. If our doubling were wrong, this is the argument that
/// would arrive mangled — and a `%` in the installation path would then break every
/// click of a real install, silently.
fn probe_channel() -> PathBuf {
    PathBuf::from(format!(
        r"\\.\pipe\universallink-menu-live-100%-{}",
        std::process::id()
    ))
}

/// A fake manager: it accepts couriers on the channel and records what they asked
/// for. Everything the real manager does with a request is tested elsewhere; what is
/// under test here is whether the request arrives at all, and with what.
struct Probe {
    path: PathBuf,
    seen: Arc<Mutex<Vec<Request>>>,
    _task: tokio::task::JoinHandle<()>,
}

impl Probe {
    async fn start() -> Probe {
        let path = probe_channel();
        let mut listener = channel::bind(&path).expect("bind the probe channel");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let task = tokio::spawn(async move {
            while let Ok(mut stream) = listener.accept().await {
                if let Ok(request) = channel::read_request(&mut stream).await {
                    recorded.lock().expect("lock").push(request);
                }
                channel::write_response(
                    &mut stream,
                    &Response::Accepted {
                        transfer_id: "t_live".into(),
                    },
                )
                .await;
            }
        });
        Probe {
            path,
            seen,
            _task: task,
        }
    }

    /// Waits for at least one request, then returns everything seen.
    async fn requests(&self) -> Vec<Request> {
        let deadline = tokio::time::Instant::now() + CLICK_TIMEOUT;
        loop {
            let seen = self.seen.lock().expect("lock").clone();
            if !seen.is_empty() {
                // A burst may still be arriving: give the rest a moment.
                tokio::time::sleep(Duration::from_millis(300)).await;
                return self.seen.lock().expect("lock").clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no courier ever reached the probe channel"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// The helper command a live artifact must carry: the courier binary this test
/// built, pointed at the probe's channel.
fn helper(channel: &Path) -> HelperCommand {
    HelperCommand {
        program: PathBuf::from(env!("CARGO_BIN_EXE_universallink-menu")),
        extra_args: vec!["--channel".into(), channel.to_string_lossy().into_owned()],
    }
}

fn a_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"hello").expect("write");
    path
}

/// Runs a PowerShell script and returns its output, failing the test if it could
/// not be started.
fn powershell(script: &str) -> String {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .expect("powershell.exe");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "powershell failed: {}\n{stdout}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// A temporary top-level verb, removed when dropped. Used to have the real shell
/// invoke a command line for us — a cascade CHILD cannot be reached that way (see
/// the module header).
struct ProbeVerb {
    key: String,
}

impl ProbeVerb {
    const LABEL: &'static str = "UL-LIVE-PROBE";

    /// Registers `command` under a verb of our own on all files.
    fn register(command: &str) -> ProbeVerb {
        let key = format!(r"HKCU\{CLASSES}\*\shell\ULLiveProbe");
        let ok = Command::new("reg.exe")
            .args(["add", &key, "/ve", "/t", "REG_SZ", "/d", Self::LABEL, "/f"])
            .status()
            .expect("reg.exe");
        assert!(ok.success(), "cannot register the probe verb");
        let ok = Command::new("reg.exe")
            .args([
                "add",
                &format!(r"{key}\command"),
                "/ve",
                "/t",
                "REG_SZ",
                "/d",
                command,
                "/f",
            ])
            .status()
            .expect("reg.exe");
        assert!(ok.success(), "cannot set the probe verb's command");
        ProbeVerb { key }
    }

    /// Asks the shell to invoke it on `file`, exactly as a click would.
    fn invoke(&self, file: &Path) {
        let script = format!(
            r#"$sh = New-Object -ComObject Shell.Application
$item = $sh.Namespace('{}').ParseName('{}')
$verb = $item.Verbs() | Where-Object {{ $_.Name -eq '{}' }}
if (-not $verb) {{ throw 'the shell did not offer the probe verb' }}
$verb.DoIt()"#,
            file.parent().expect("parent").display(),
            file.file_name().expect("name").to_string_lossy(),
            Self::LABEL
        );
        powershell(&script);
    }
}

impl Drop for ProbeVerb {
    fn drop(&mut self) {
        let _ = Command::new("reg.exe")
            .args(["delete", &self.key, "/f"])
            .status();
    }
}

/// How many items the real shell puts in a file's context menu.
///
/// Indirect on purpose: `Shell.Application` reports a submenu with an empty name, so
/// what can be observed from outside is that the shell built ONE MORE item — which
/// is precisely the question about `ExtendedSubCommandsKey` under `HKEY_CURRENT_USER`
/// (does Explorer honour it at all, without an administrator and without the
/// machine-wide `CommandStore`).
fn menu_item_count(file: &Path) -> usize {
    let script = format!(
        r#"$sh = New-Object -ComObject Shell.Application
$item = $sh.Namespace('{}').ParseName('{}')
$item.Verbs().Count"#,
        file.parent().expect("parent").display(),
        file.file_name().expect("name").to_string_lossy()
    );
    powershell(&script).trim().parse().expect("a verb count")
}

/// The command line we write into the registry, run by the REAL shell.
///
/// This is the test that closes the questions no documentation answers: whether our
/// quoting survives the shell's own parse, whether `%%` really becomes one percent
/// sign, and whether `"%1"` is replaced by the selected file. It also proves a
/// courier the SHELL started can reach the manager at all — including that a
/// GUI-subsystem process (no console window to flash) still works.
#[tokio::test]
#[ignore = "needs a real Windows session: it registers a verb and asks the shell to run it"]
async fn the_real_shell_runs_the_command_line_we_write() {
    let probe = Probe::start().await;
    let command = verb_command_line(&helper(&probe.path), &target());
    let verb = ProbeVerb::register(&command);

    let dir = tempfile::tempdir().expect("tempdir");
    // A name with a space and a percent sign of its own: the substitution has to
    // survive both.
    let file = a_file(dir.path(), "a file 100%.txt");
    verb.invoke(&file);

    let requests = probe.requests().await;
    assert_eq!(
        requests,
        vec![Request::Send {
            device_id: "d_livetest".into(),
            paths: vec![file.clone()],
        }],
        "the shell's own invocation must reach us as this exact click"
    );
}

/// Whether Explorer honours a per-user `ExtendedSubCommandsKey` cascade at all.
///
/// The alternative form of a cascade resolves its children from a `CommandStore`
/// under `HKEY_LOCAL_MACHINE`, which an unprivileged component cannot write — so if
/// this were not honoured, the whole surface would have to be redesigned as a flat
/// list of verbs.
#[tokio::test]
#[ignore = "needs a real Windows session: it writes to the user's real shortcut menu"]
async fn the_real_shell_shows_the_cascade_and_forgets_it_when_it_goes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = a_file(dir.path(), "a.txt");
    let before = menu_item_count(&file);

    let mut cascade = Cascade::files(CLASSES, helper(&probe_channel()));
    cascade.apply(&[target()]).expect("apply");
    let with = menu_item_count(&file);
    // Removed before the assertion: a panic must not leave an entry behind on the
    // developer's own machine.
    cascade.apply(&[]).expect("clear");
    let after = menu_item_count(&file);

    assert_eq!(
        with,
        before + 1,
        "the shell ignored the cascade: {before} items, then {with}"
    );
    assert_eq!(after, before, "the cascade outlived its removal");
}

/// The Send to surface, in the folder the shell really reads — and the entries of
/// the user's own that must survive our sweep.
#[tokio::test]
#[ignore = "needs a real Windows session: it writes into the user's real Send to folder"]
async fn a_send_to_entry_lands_in_the_real_folder_and_leaves_the_others_alone() {
    let folder = universallink_menu::os::windows::send_to_folder().expect("the SendTo folder");
    let before: Vec<PathBuf> = std::fs::read_dir(&folder)
        .expect("read the SendTo folder")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    assert!(
        !before.is_empty(),
        "a real SendTo folder is never empty: {}",
        folder.display()
    );

    let mut surface = SendTo::new(&folder, helper(&probe_channel()));
    surface.apply(&[target()]).expect("apply");
    let entry = folder.join("PC-Live & Co (UniversalLink).lnk");
    let created = entry.exists();
    surface.apply(&[]).expect("clear");

    assert!(created, "no shortcut at {}", entry.display());
    assert!(!entry.exists(), "the shortcut outlived its removal");
    for path in before {
        assert!(
            path.exists(),
            "an entry of the user's own was swept: {}",
            path.display()
        );
    }
}
