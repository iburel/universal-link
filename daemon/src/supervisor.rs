// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The supervisor of the official components: it launches them, restarts them
//! when they fall over, and takes them down when the Core stops.
//!
//! # Contract of a supervised component
//!
//! 1. It finds the Core at the path passed in `ONEDEVICE_IPC_PATH`.
//! 2. It reads its **spawn token** from the first line of its standard input.
//!    Neither `argv` (readable by everyone in `/proc/pid/cmdline`) nor the
//!    environment (inherited by all of its descendants).
//! 3. Its standard input stays open. **Its EOF means "stop"**: it is the only
//!    graceful-shutdown channel that exists on all three OSes.
//! 4. **If it loses its IPC connection, it must exit.** The spawn token is
//!    single-use: it will not be able to reconnect with it. The supervisor
//!    will restart it with a fresh token. A component that looped on
//!    reconnections doomed to fail would be a live and useless process, which
//!    process supervision would not be able to detect.
//!
//! The token is minted at each launch and **taken back as soon as the child
//! dies**. Without that reclamation, a child that crashes before its `hello`
//! would leave an activation token alive until the Core stops — one more with
//! each restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use onedevice_core::CoreHandle;
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use crate::child;

/// An official component: what we launch, and with what rights.
#[derive(Clone, Debug)]
pub struct ChildSpec {
    /// Executable. Looked up next to the Core binary.
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Role requested at `hello` — the spawn token is bound to it.
    pub role: String,
    /// Scopes granted by the token. The child may request fewer.
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Policy {
    /// First wait before restarting; doubled at each consecutive failure.
    pub restart_base_delay: Duration,
    pub restart_max_delay: Duration,
    /// A child that has held on for at least this long is deemed healthy: the
    /// next crash starts over from the base delay.
    pub healthy_after: Duration,
    /// Delay allowed at each step of the shutdown (EOF, then SIGTERM).
    pub grace: Duration,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            restart_base_delay: Duration::from_millis(500),
            restart_max_delay: Duration::from_secs(60),
            healthy_after: Duration::from_secs(10),
            grace: Duration::from_secs(3),
        }
    }
}

pub struct Supervisor {
    stop: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Supervisor {
    pub fn start(
        core: Arc<CoreHandle>,
        ipc_path: PathBuf,
        specs: Vec<ChildSpec>,
        policy: Policy,
    ) -> Supervisor {
        let (stop, _) = watch::channel(false);
        let tasks = specs
            .into_iter()
            .map(|spec| {
                let core = core.clone();
                let ipc_path = ipc_path.clone();
                let policy = policy.clone();
                let stop_rx = stop.subscribe();
                tokio::spawn(supervise(core, ipc_path, spec, policy, stop_rx))
            })
            .collect();
        Supervisor { stop, tasks }
    }

    /// Stops restarting, stops each child, and waits for them to be reaped —
    /// the tokio runtime must still be running for that, hence the order
    /// imposed in `main`: supervisor first, Core after.
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

async fn supervise(
    core: Arc<CoreHandle>,
    ipc_path: PathBuf,
    spec: ChildSpec,
    policy: Policy,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut delay = policy.restart_base_delay;
    loop {
        if *stop_rx.borrow_and_update() {
            return;
        }
        let started = Instant::now();
        let stopped = run_once(&core, &ipc_path, &spec, &policy, &mut stop_rx).await;
        if stopped {
            return;
        }
        // A child that has stood up for a while deserves an immediate restart;
        // the backoff only punishes crashes in bursts.
        if started.elapsed() >= policy.healthy_after {
            delay = policy.restart_base_delay;
        }
        tracing::info!(
            role = %spec.role,
            delay_ms = delay.as_millis() as u64,
            "restarting the component"
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stop_rx.changed() => return,
        }
        delay = (delay * 2).min(policy.restart_max_delay);
    }
}

/// Launches the child, sees it through to its death. `true`: it was the Core's
/// shutdown that took it down, there is nothing to restart.
async fn run_once(
    core: &CoreHandle,
    ipc_path: &std::path::Path,
    spec: &ChildSpec,
    policy: &Policy,
    stop_rx: &mut watch::Receiver<bool>,
) -> bool {
    let scopes: Vec<&str> = spec.scopes.iter().map(String::as_str).collect();
    let token = core.mint_spawn_token(&spec.role, &scopes);

    let envs = [(
        "ONEDEVICE_IPC_PATH",
        ipc_path.to_string_lossy().into_owned(),
    )];
    let mut handle = match child::spawn(&spec.program, &spec.args, &envs) {
        Ok(handle) => handle,
        Err(e) => {
            // The token will go nowhere: take it back right away.
            core.revoke_spawn_token(&token);
            tracing::error!(
                role = %spec.role,
                program = %spec.program.display(),
                error = %e,
                "could not launch the component"
            );
            return false;
        }
    };

    if let Some(stdin) = handle.stdin.as_mut()
        && let Err(e) = stdin.write_all(format!("{token}\n").as_bytes()).await
    {
        tracing::error!(role = %spec.role, error = %e, "token not passed to the component");
    }

    tracing::info!(role = %spec.role, "component launched");
    let stopped = tokio::select! {
        status = handle.child.wait() => {
            // The child's descendants outlive it: sweep them before
            // restarting, otherwise two generations of shims would coexist.
            child::sweep(&handle);
            match status {
                Ok(status) => tracing::warn!(role = %spec.role, %status, "component exited"),
                Err(e) => tracing::error!(role = %spec.role, error = %e, "component lost"),
            }
            false
        }
        _ = stop_rx.changed() => {
            child::stop(&mut handle, policy.grace).await;
            tracing::info!(role = %spec.role, "component stopped");
            true
        }
    };

    // No effect if the child consumed it at its `hello`: a spawn token is
    // used only once. Has an effect if it died before — that is the whole
    // point.
    core.revoke_spawn_token(&token);
    stopped
}

/// The deployment's official components, looked up next to the Core binary — or,
/// for the tray, in the helper bundle described by `helper_bundle_program`.
/// The tray is registered on every platform; the clipboard backend is registered
/// on Linux (X11), Windows, and macOS, and so are the contextual menu, the sync
/// engine and the keyboard and mouse engine.
/// A missing executable is ignored (with a word in the log) — a Core without a
/// tray is still a working Core.
///
/// The tray is granted `system.shutdown` (its Quit stops the whole Core) and
/// `input.read` (the keyboard and mouse session it shows for as long as it lasts)
/// on top of `session.read` (its status icon); it requests only what each of its
/// building blocks actually uses.
pub fn official_components() -> Vec<ChildSpec> {
    // (name, role, scopes). The tray is cross-platform; the clipboard backend
    // has a Linux/X11 (brick 2), a Windows (brick 3), and a macOS (brick 4)
    // backend, so it is registered on all three.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "windows", target_os = "macos")),
        allow(unused_mut)
    )]
    // `input.read` is what lets the tray show a keyboard that is away, on both
    // sides, for as long as the session lasts (the epic's rule). Read only: the
    // tray never grants anything and never takes a keyboard. It is asked for
    // OPTIONALLY on the tray's side, so a tray and a Core that ever disagree
    // about this list lose the input line rather than the whole tray.
    let mut official: Vec<(&str, &str, &[&str])> = vec![(
        "1device-tray",
        "tray",
        &["session.read", "system.shutdown", "input.read"],
    )];
    #[cfg(target_os = "linux")]
    official.push((
        "1device-clipboard",
        "clipboard-backend",
        &["devices.read", "clipboard.read", "clipboard.write"],
    ));
    #[cfg(target_os = "windows")]
    official.push((
        "1device-clipboard",
        "clipboard-backend",
        &["devices.read", "clipboard.read", "clipboard.write"],
    ));
    // macOS pastes files via `transactions.fill`, whose completion is reported
    // out-of-band on the `transfers` topic — hence the extra `transfers.read`
    // (brick 7). The pull-at-paste platforms above do not need it.
    #[cfg(target_os = "macos")]
    official.push((
        "1device-clipboard",
        "clipboard-backend",
        &[
            "devices.read",
            "clipboard.read",
            "clipboard.write",
            "transfers.read",
        ],
    ));
    // The contextual menu, on the platforms where it has a surface: Linux (KDE
    // ServiceMenus + Nautilus scripts), Windows and macOS. Registering it where it
    // has none would relaunch a process that exits immediately, for ever. It reads
    // the session (an entry must not be offered while the Core is offline: the
    // directory it serves from cache would have us point at unreachable devices),
    // the devices, and sends on a click.
    #[cfg(target_os = "linux")]
    official.push((
        "1device-menu",
        "menu-backend",
        &["session.read", "devices.read", "files.send"],
    ));
    // Windows (brick 3): the same rights, for the classic shortcut menu's cascade
    // and the "Send to" shortcuts.
    #[cfg(target_os = "windows")]
    official.push((
        "1device-menu",
        "menu-backend",
        &["session.read", "devices.read", "files.send"],
    ));
    // macOS (brick 4): the same rights again, for the Automator workflows in
    // `~/Library/Services` that Finder shows in its Services submenu (the inline
    // Quick Actions row is a bonus the user enables, see `menu::os::macos`).
    #[cfg(target_os = "macos")]
    official.push((
        "1device-menu",
        "menu-backend",
        &["session.read", "devices.read", "files.send"],
    ));
    // The sync engine: the exclusive `sync-backend` behind the routed `sync.*`
    // facade (doc/sync-engine.md). Computers only (the v1 limits), with the
    // documented profile everywhere - one push for the three desktops.
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    official.push((
        "1device-sync",
        "sync-backend",
        &[
            "sync.serve",
            "transactions.publish",
            "peers.message",
            "devices.read",
            "session.read",
            "transfers.read",
        ],
    ));
    // The keyboard and mouse engine: the exclusive `input-backend` behind the
    // routed `input.*` facade (doc/input-sharing.md). Computers only, and for a
    // blunter reason than the sync engine's: a phone has no pointer to lend and
    // nothing to capture. It carries the events over `peers.channel` (a standing
    // pipe, since a mouse move every 8 ms cannot pay for an ack apiece) and
    // keeps `peers.message` for the occasional word out of band.
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    official.push((
        "1device-input",
        "input-backend",
        &[
            "input.serve",
            "peers.channel",
            "peers.message",
            "devices.read",
            "session.read",
        ],
    ));

    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    else {
        tracing::warn!("binary path unknown: no component will be launched");
        return Vec::new();
    };

    official
        .iter()
        .filter_map(|(name, role, scopes)| {
            let Some(program) = component_program(&dir, name) else {
                tracing::info!(component = name, "component absent: not launched");
                return None;
            };
            Some(ChildSpec {
                program,
                args: Vec::new(),
                role: (*role).to_string(),
                scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            })
        })
        .collect()
}

/// Where a component's executable is: in its own helper bundle if the build
/// ships one, otherwise next to the Core. `None` = this build ships without it.
fn component_program(dir: &Path, name: &str) -> Option<PathBuf> {
    helper_bundle_program(dir, name)
        .into_iter()
        .chain(std::iter::once(dir.join(executable_name(name))))
        .find(|program| program.exists())
}

/// A component that must **not** be attributed to the application bundle it sits
/// in gets a nested bundle of its own; this is where it lands.
///
/// Only the tray, and in practice only on macOS, where the reason is Launch
/// Services: a process started from `1Device.app/Contents/MacOS` *is* the
/// application, and takes the enclosing bundle's identifier. The tray owns a
/// status item, so it checks in as a GUI application — and from the moment the
/// Core spawned it, `org.onedevice.gui` was already running. Every `open` of
/// the app (the Dock, the Finder, the Launchpad, and the tray's own Open item)
/// then activated the tray, which has no window, so clicking the icon did
/// nothing whatsoever. Inside `Contents/Frameworks/1DeviceTray.app` the
/// nearest enclosing bundle is its own: the layout Chromium and Electron helpers
/// have always used. `tray/macos/Info.plist` is that bundle's identity and
/// `release.yml` assembles it.
///
/// Deliberately not conditional on the platform: the path simply does not exist
/// on Linux or Windows, where the sidecars sit beside the Core, and one lookup
/// with one set of tests is worth more here than a `cfg`.
fn helper_bundle_program(dir: &Path, name: &str) -> Option<PathBuf> {
    let bundle = match name {
        "1device-tray" => "1DeviceTray.app",
        _ => return None,
    };
    // `dir` is `Contents/MacOS`; the helper is its sibling under `Contents`. The
    // executable's name is the bundle's `CFBundleExecutable`, hence no `.exe`
    // dance: this layout is macOS's alone.
    Some(
        dir.parent()?
            .join("Frameworks")
            .join(bundle)
            .join("Contents")
            .join("MacOS")
            .join(name),
    )
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod lookup_tests {
    use super::{component_program, executable_name};

    /// `Contents/MacOS` with the Core in it, and whatever else the test asks for.
    fn bundle(dir: &std::path::Path) -> std::path::PathBuf {
        let macos = dir.join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos).expect("create Contents/MacOS");
        macos
    }

    fn touch(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the parent");
        std::fs::write(path, b"").expect("write the executable");
    }

    #[test]
    fn a_component_beside_the_core_is_what_gets_launched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let macos = bundle(tmp.path());
        let sibling = macos.join(executable_name("1device-tray"));
        touch(&sibling);

        assert_eq!(component_program(&macos, "1device-tray"), Some(sibling));
    }

    /// The macOS layout, and the reason this lookup exists: the tray in a bundle
    /// of its own is preferred to the copy beside the Core, which is the one that
    /// used to steal the application's Launch Services identity.
    #[test]
    fn the_trays_helper_bundle_wins_over_a_copy_beside_the_core() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let macos = bundle(tmp.path());
        touch(&macos.join(executable_name("1device-tray")));
        let helper = tmp
            .path()
            .join("Contents/Frameworks/1DeviceTray.app/Contents/MacOS/1device-tray");
        touch(&helper);

        assert_eq!(component_program(&macos, "1device-tray"), Some(helper));
    }

    /// Nothing else moves: the helper layout is the tray's alone, so a clipboard
    /// backend is taken from beside the Core even if that path exists.
    #[test]
    fn no_other_component_is_looked_for_in_a_helper_bundle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let macos = bundle(tmp.path());
        let decoy = tmp
            .path()
            .join("Contents/Frameworks/1DeviceTray.app/Contents/MacOS/1device-clipboard");
        touch(&decoy);

        assert_eq!(component_program(&macos, "1device-clipboard"), None);

        let sibling = macos.join(executable_name("1device-clipboard"));
        touch(&sibling);
        assert_eq!(
            component_program(&macos, "1device-clipboard"),
            Some(sibling)
        );
    }

    #[test]
    fn a_component_this_build_does_not_ship_is_not_launched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let macos = bundle(tmp.path());

        assert_eq!(component_program(&macos, "1device-tray"), None);
    }

    /// A Core that is not inside a bundle at all (a `cargo run`, and every Linux
    /// and Windows install) still resolves its components — the helper candidate
    /// is simply a path that does not exist.
    #[test]
    fn a_core_outside_any_bundle_still_finds_its_components() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sibling = tmp.path().join(executable_name("1device-tray"));
        touch(&sibling);

        assert_eq!(component_program(tmp.path(), "1device-tray"), Some(sibling));
    }
}
