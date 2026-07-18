// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The supervisor of the official components: it launches them, restarts them
//! when they fall over, and takes them down when the Core stops.
//!
//! # Contract of a supervised component
//!
//! 1. It finds the Core at the path passed in `UNIVERSALLINK_IPC_PATH`.
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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use universallink_core::CoreHandle;

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
        "UNIVERSALLINK_IPC_PATH",
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

/// The deployment's official components, looked up next to the Core binary.
/// The tray is registered on every platform; the clipboard backend is registered
/// on Linux (X11), Windows, and macOS. The contextual menu will come and register
/// here too. A missing executable is ignored (with a word in the log) — a Core
/// without a tray is still a working Core.
///
/// The tray is granted `system.shutdown` (its Quit stops the whole Core) on top
/// of `session.read` (its status icon); it requests only what each of its
/// building blocks actually uses.
pub fn official_components() -> Vec<ChildSpec> {
    // (name, role, scopes). The tray is cross-platform; the clipboard backend
    // has a Linux/X11 (brick 2), a Windows (brick 3), and a macOS (brick 4)
    // backend, so it is registered on all three.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "windows", target_os = "macos")),
        allow(unused_mut)
    )]
    let mut official: Vec<(&str, &str, &[&str])> = vec![(
        "universallink-tray",
        "tray",
        &["session.read", "system.shutdown"],
    )];
    #[cfg(target_os = "linux")]
    official.push((
        "universallink-clipboard",
        "clipboard-backend",
        &["devices.read", "clipboard.read", "clipboard.write"],
    ));
    #[cfg(target_os = "windows")]
    official.push((
        "universallink-clipboard",
        "clipboard-backend",
        &["devices.read", "clipboard.read", "clipboard.write"],
    ));
    // macOS pastes files via `transactions.fill`, whose completion is reported
    // out-of-band on the `transfers` topic — hence the extra `transfers.read`
    // (brick 7). The pull-at-paste platforms above do not need it.
    #[cfg(target_os = "macos")]
    official.push((
        "universallink-clipboard",
        "clipboard-backend",
        &[
            "devices.read",
            "clipboard.read",
            "clipboard.write",
            "transfers.read",
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
            let program = dir.join(executable_name(name));
            if !program.exists() {
                tracing::info!(component = name, "component absent: not launched");
                return None;
            }
            Some(ChildSpec {
                program,
                args: Vec::new(),
                role: (*role).to_string(),
                scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            })
        })
        .collect()
}

fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}
