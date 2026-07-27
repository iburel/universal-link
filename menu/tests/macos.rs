// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Live macOS tests for the contextual-menu surface.
//!
//! These write to the REAL `~/Library/Services`, and they ask the REAL system what it
//! makes of what they wrote: the services registry (`pbs`), the workflow runtime
//! (`automator`), and the service lookup AppKit performs by label. Each one puts the
//! machine back as it found it — and only ever removes bundles carrying our marker,
//! which is the surface's own rule.
//!
//! All are `#[ignore]`d: the workspace CI runs `cargo test` without `--ignored`, so
//! they are not part of the automated suite. They are run by hand on a real macOS
//! session with:
//!
//!     cargo test -p universallink-menu --test macos -- --ignored --test-threads=1
//!
//! Single-threaded, because they share one namespace: the user's own services.
//!
//! # What is left to a human, and why
//!
//! One thing: **that the entry is visible in a real contextual menu**. Everything up
//! to that is checked here — the system registered the service, under the label we
//! wrote, and it runs our document — but whether Finder DRAWS it can only be seen by
//! opening the menu. It is also the one place where macOS may ask something of the
//! user: the Services submenu needs nothing, while the inline "Quick Actions" row of
//! the same menu may need a checkbox in System Settings that no code can tick.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use universallink_menu::channel::{self, Request, Response};
use universallink_menu::os::macos::{Services, services_dir};
use universallink_menu::{HelperCommand, MenuSurface, Target};

/// How long the system is given to notice a service appearing or disappearing. It
/// notices an appearance on its own within a second or so; the surface asks for both
/// explicitly, so this only has to be well clear of a busy machine.
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a click is given to reach the probe.
const CLICK_TIMEOUT: Duration = Duration::from_secs(30);
const PBS: &str = "/System/Library/CoreServices/pbs";

fn target() -> Target {
    Target {
        device_id: "d_livetest".into(),
        // Deliberately holding an ampersand: the label goes into the plist escaped,
        // and what proves the escaping is that the SYSTEM's own parser gives the
        // ampersand back — which is what the registry dump below shows.
        name: "PC-Live & Co".into(),
        platform: "macos".into(),
    }
}

fn label() -> String {
    format!("Send to {} (UniversalLink)", target().name)
}

/// The real surface, and a guarantee that it is emptied again whatever the test does.
struct RealMenu {
    surface: Services,
}

impl RealMenu {
    fn open(channel: &Path) -> RealMenu {
        let dir = services_dir().expect("a home directory");
        std::fs::create_dir_all(&dir).expect("the services directory");
        RealMenu {
            surface: Services::new(&dir, helper(channel)),
        }
    }

    fn apply(&mut self, targets: &[Target]) {
        self.surface.apply(targets).expect("apply");
    }

    /// Our own bundles, by the naming the surface uses.
    fn bundles(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.surface.dir()) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("UniversalLink-") && n.ends_with(".workflow"))
            })
            .collect()
    }
}

impl Drop for RealMenu {
    fn drop(&mut self) {
        // Exactly what a graceful shutdown does — including telling the system, so a
        // failed test does not leave an entry in the developer's menu.
        let _ = self.surface.apply(&[]);
    }
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
    /// `tag` keeps each test on a channel of its own: a courier the system started
    /// late — after the test that provoked it finished — must not be counted by the
    /// next one.
    async fn start(tag: &str) -> Probe {
        // A path with a SPACE in it: the script single-quotes what it carries, and a
        // real installation path (`/Applications/UniversalLink.app/…`) is one word
        // only by luck.
        let path =
            std::env::temp_dir().join(format!("ul menu live {tag} {}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
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
            if !self.seen.lock().expect("lock").is_empty() {
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

/// The helper command a live artifact must carry: the courier binary this test built,
/// pointed at the probe's channel.
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

/// Everything the system currently has registered as a service.
fn registered() -> String {
    let out = Command::new(PBS).arg("-dump_pboard").output().expect("pbs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

async fn await_registration(label: &str, present: bool) {
    let deadline = tokio::time::Instant::now() + REGISTRY_TIMEOUT;
    loop {
        if registered().contains(label) == present {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{label:?} is {}registered and should {}be",
            if present { "not " } else { "" },
            if present { "" } else { "not " }
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Registration, through the real thing: the system parsed the `Info.plist` we wrote
/// and offers a service under the label we asked for — ampersand and all, which is the
/// XML escaping being read back by the system's own parser — then follows a rename, and
/// stops offering anything when the list empties.
///
/// What this does NOT prove is that `NSUpdateDynamicServices` did anything: macOS
/// reaches the same end state on its own in about seven seconds, well inside
/// [`REGISTRY_TIMEOUT`], so removing the call keeps this test green (checked, by
/// mutation). That call is a latency fix and this is an end-state test; the two do not
/// meet, and pinning the timing instead would pin the machine's mood.
#[tokio::test]
#[ignore = "writes to the real ~/Library/Services"]
async fn the_real_services_registry_follows_our_entries() {
    let probe = Probe::start("registry").await;
    let mut menu = RealMenu::open(&probe.path);

    menu.apply(&[target()]);
    assert_eq!(menu.bundles().len(), 1, "{:?}", menu.bundles());
    await_registration(&label(), true).await;

    // A rename: the same device, the same bundle, one plist rewritten in place. The
    // menu has to show the new label and stop showing the old one.
    let renamed = Target {
        name: "PC-Live renamed".into(),
        ..target()
    };
    menu.apply(std::slice::from_ref(&renamed));
    assert_eq!(menu.bundles().len(), 1, "the rename moved the bundle");
    await_registration(&format!("Send to {} (UniversalLink)", renamed.name), true).await;
    await_registration(&label(), false).await;

    menu.apply(&[]);
    assert_eq!(menu.bundles().len(), 0);
    await_registration(&format!("Send to {} (UniversalLink)", renamed.name), false).await;
}

/// The document, through the real workflow runtime: `automator` runs the bundle we
/// wrote, its "Run Shell Script" action starts the real courier, and the courier
/// reaches the manager asking to send that file to that device.
///
/// This is what proves the hand-written document is one the engine accepts — `plutil`
/// only proves it is a plist.
#[tokio::test]
#[ignore = "writes to the real ~/Library/Services and runs Automator"]
async fn the_real_workflow_runtime_runs_our_document() {
    let probe = Probe::start("runtime").await;
    let mut menu = RealMenu::open(&probe.path);
    menu.apply(&[target()]);

    let dir = tempfile::tempdir().expect("tempdir");
    let file = a_file(dir.path(), "a note.txt");
    let bundle = menu.bundles().pop().expect("our bundle");

    // `tokio::process`, never `std::process`: this waits for the whole workflow, and
    // the workflow waits for the courier the probe in THIS process has to answer.
    // Blocking the runtime here deadlocks all three.
    let out = tokio::time::timeout(
        CLICK_TIMEOUT,
        tokio::process::Command::new("/usr/bin/automator")
            .arg("-i")
            .arg(&file)
            .arg(&bundle)
            .output(),
    )
    .await
    .expect("automator did not finish in time")
    .expect("automator");
    assert!(
        out.status.success(),
        "automator refused the workflow: {} / {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let requests = probe.requests().await;
    assert_eq!(requests.len(), 1, "{requests:?}");
    match &requests[0] {
        Request::Send { device_id, paths } => {
            assert_eq!(device_id, &target().device_id);
            assert_eq!(paths, &[file]);
        }
        other => panic!("the courier asked for {other:?}"),
    }
}

/// The lookup, through AppKit: asking the system to perform the service BY THE LABEL
/// we wrote resolves it and matches a file selection — so the label we put in a menu
/// is the one the system knows the service by, and a selection of files is something
/// it accepts.
///
/// A made-up label is the control: without it, a `true` here would mean nothing. What
/// this does NOT prove is that the workflow then ran — dispatch launches the runner in
/// the graphical session, which a test process may not be in. Execution is the test
/// above.
#[tokio::test]
#[ignore = "writes to the real ~/Library/Services"]
async fn a_lookup_by_the_label_we_wrote_resolves_the_service() {
    let probe = Probe::start("lookup").await;
    let mut menu = RealMenu::open(&probe.path);
    menu.apply(&[target()]);
    await_registration(&label(), true).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let file = a_file(dir.path(), "a note.txt");
    assert_eq!(
        perform_service(&label(), &file),
        "true",
        "the system did not resolve the label we wrote"
    );
    assert_eq!(
        perform_service("Send to Nobody (UniversalLink)", &file),
        "false",
        "the system resolved a label nothing wrote"
    );
}

/// Asks AppKit to perform the service named `label` on a pasteboard holding `file`.
/// Returns "true" or "false" — whatever `NSPerformService` answered.
fn perform_service(label: &str, file: &Path) -> String {
    let script = format!(
        r#"ObjC.import('AppKit');
var pb = $.NSPasteboard.pasteboardWithUniqueName;
pb.clearContents;
var items = $.NSMutableArray.alloc.init;
items.addObject($.NSURL.fileURLWithPath({file}));
pb.writeObjects(items);
String($.NSPerformService($({label}), pb))"#,
        // Through JSON, so a quote or a backslash in either cannot end the literal.
        file = json_string(&file.to_string_lossy()),
        label = json_string(label),
    );
    let out = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", &script])
        .output()
        .expect("osascript");
    assert!(
        out.status.success(),
        "osascript failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A JavaScript string literal for `s`. JSON is a subset of JavaScript, and
/// `serde_json` is already here.
fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}
