// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Production conventions: where the Core listens, where it stores its files.
//!
//! DECISION (building block G2, extracted here by the daemon building block) —
//! these paths are THE local, per-user deployment contract. The daemon listens
//! and writes exactly here; the official components (GUI, then tray, menu)
//! read the same paths.
//!
//! | OS      | socket / pipe                                          | config directory |
//! |---------|--------------------------------------------------------|------------------|
//! | Linux   | `$XDG_RUNTIME_DIR/universallink/core.sock`              | `$XDG_CONFIG_HOME` (default `~/.config`) `/universallink` |
//! | macOS   | `~/Library/Application Support/UniversalLink/core.sock` | same as the socket |
//! | Windows | `\\.\pipe\universallink-core-<USERDOMAIN>-<USERNAME>`   | `%APPDATA%\UniversalLink` |
//!
//! The config directory holds `ipc-token`, `device.key`, `session.json`,
//! `secrets.json` (keyring fallback) and `config.json`. The single-instance
//! lock, for its part, lives next to the socket (`core.sock.lock`): it is the
//! socket it protects — see `universallink_core::transport`.

use std::path::PathBuf;

pub struct Endpoint {
    /// The Core's listening endpoint (UDS socket or named pipe name).
    pub ipc_path: PathBuf,
    /// The Core's config directory. The file token (`ipc-token`) is
    /// regenerated here at every startup — re-read on every attempt by the
    /// client.
    pub config_dir: PathBuf,
}

impl Endpoint {
    pub fn token_path(&self) -> PathBuf {
        self.config_dir.join("ipc-token")
    }

    /// Where the GUI records how the tray should relaunch it. The tray runs
    /// from the Core's durable copy and cannot otherwise find the GUI — a loose
    /// AppImage on Linux especially. Written by the GUI, read by the tray.
    pub fn gui_launch_path(&self) -> PathBuf {
        self.config_dir.join("gui-launch")
    }
}

/// Production paths, resolved from the process's environment.
/// `None`: unusable environment (an indispensable variable is absent) — to be
/// treated as a startup error, not to be guessed.
pub fn production_endpoint() -> Option<Endpoint> {
    endpoint_from(&|key| std::env::var(key).ok())
}

/// Where the daemon writes its log. Separate from the config directory: this
/// is volatile data, not settings. `None`: unusable environment — the daemon
/// will then make do with its error output.
pub fn log_dir() -> Option<PathBuf> {
    log_dir_from(&|key| std::env::var(key).ok())
}

#[cfg(target_os = "linux")]
fn log_dir_from(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let xdg = |key: &str| env(key).filter(|v| v.starts_with('/'));
    // XDG stores state data (including logs) under XDG_STATE_HOME.
    let state = xdg("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| xdg("HOME").map(|home| PathBuf::from(home).join(".local").join("state")))?;
    Some(state.join("universallink").join("logs"))
}

#[cfg(target_os = "macos")]
fn log_dir_from(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let home = env("HOME").filter(|v| v.starts_with('/'))?;
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("Logs")
            .join("UniversalLink"),
    )
}

#[cfg(windows)]
fn log_dir_from(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    // LOCALAPPDATA and not APPDATA: a log has no business in a roaming
    // profile.
    let local = env("LOCALAPPDATA").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(local).join("UniversalLink").join("logs"))
}

#[cfg(target_os = "linux")]
fn endpoint_from(env: &dyn Fn(&str) -> Option<String>) -> Option<Endpoint> {
    // XDG Base Directory spec: an empty variable counts as absent, a relative
    // path is invalid and must be ignored — hence the absolute filter.
    let xdg = |key: &str| env(key).filter(|v| v.starts_with('/'));
    // XDG_RUNTIME_DIR is guaranteed by systemd/logind sessions; without it,
    // there is no safe location (0700, per-user) for a socket.
    let runtime = xdg("XDG_RUNTIME_DIR")?;
    let config = xdg("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| xdg("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(Endpoint {
        ipc_path: PathBuf::from(runtime)
            .join("universallink")
            .join("core.sock"),
        config_dir: config.join("universallink"),
    })
}

#[cfg(target_os = "macos")]
fn endpoint_from(env: &dyn Fn(&str) -> Option<String>) -> Option<Endpoint> {
    let home = env("HOME").filter(|v| v.starts_with('/'))?;
    let dir = PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("UniversalLink");
    Some(Endpoint {
        ipc_path: dir.join("core.sock"),
        config_dir: dir,
    })
}

#[cfg(windows)]
fn endpoint_from(env: &dyn Fn(&str) -> Option<String>) -> Option<Endpoint> {
    let var = |key: &str| env(key).filter(|v| !v.is_empty());
    // The pipe namespace is machine-global: a per-user suffix so that two
    // sessions do not step on each other. USERNAME alone is not enough — a
    // local account `john` and a domain account `CORP\john` are two distinct
    // users with the same USERNAME; USERDOMAIN (= machine name for a local
    // account) disambiguates them.
    let domain = var("USERDOMAIN")?;
    let user = var("USERNAME")?;
    let appdata = var("APPDATA")?;
    Some(Endpoint {
        ipc_path: PathBuf::from(format!(r"\\.\pipe\universallink-core-{domain}-{user}")),
        config_dir: PathBuf::from(appdata).join("UniversalLink"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_logs_go_to_the_state_dir() {
        let dir = log_dir_from(&env_of(&[("HOME", "/home/u")])).expect("logs directory");
        assert_eq!(
            dir,
            PathBuf::from("/home/u/.local/state/universallink/logs")
        );
        let dir = log_dir_from(&env_of(&[("XDG_STATE_HOME", "/state"), ("HOME", "/home/u")]))
            .expect("logs directory");
        assert_eq!(dir, PathBuf::from("/state/universallink/logs"));
        assert!(log_dir_from(&env_of(&[])).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_logs_go_to_library_logs() {
        let dir = log_dir_from(&env_of(&[("HOME", "/Users/u")])).expect("logs directory");
        assert_eq!(dir, PathBuf::from("/Users/u/Library/Logs/UniversalLink"));
        assert!(log_dir_from(&env_of(&[])).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_logs_stay_local() {
        // LOCALAPPDATA: a log does not follow the user from one machine to
        // another.
        let dir = log_dir_from(&env_of(&[("LOCALAPPDATA", r"C:\Users\iwan\AppData\Local")]))
            .expect("logs directory");
        assert_eq!(
            dir,
            PathBuf::from(r"C:\Users\iwan\AppData\Local\UniversalLink\logs")
        );
        assert!(log_dir_from(&env_of(&[("APPDATA", r"C:\x")])).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_follows_xdg() {
        let e = endpoint_from(&env_of(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_CONFIG_HOME", "/home/u/.config-custom"),
            ("HOME", "/home/u"),
        ]))
        .expect("endpoint");
        assert_eq!(
            e.ipc_path,
            PathBuf::from("/run/user/1000/universallink/core.sock")
        );
        assert_eq!(
            e.token_path(),
            PathBuf::from("/home/u/.config-custom/universallink/ipc-token")
        );

        // Without XDG_CONFIG_HOME: ~/.config. Without XDG_RUNTIME_DIR: refusal.
        let e = endpoint_from(&env_of(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HOME", "/home/u"),
        ]))
        .expect("endpoint");
        assert_eq!(
            e.token_path(),
            PathBuf::from("/home/u/.config/universallink/ipc-token")
        );
        assert!(endpoint_from(&env_of(&[("HOME", "/home/u")])).is_none());

        // XDG spec: an EMPTY variable = absent (default), a RELATIVE path =
        // invalid (ignored) — never a silent relative path.
        let e = endpoint_from(&env_of(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_CONFIG_HOME", ""),
            ("HOME", "/home/u"),
        ]))
        .expect("endpoint");
        assert_eq!(
            e.token_path(),
            PathBuf::from("/home/u/.config/universallink/ipc-token")
        );
        let e = endpoint_from(&env_of(&[
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_CONFIG_HOME", "relative-config"),
            ("HOME", "/home/u"),
        ]))
        .expect("endpoint");
        assert_eq!(
            e.token_path(),
            PathBuf::from("/home/u/.config/universallink/ipc-token")
        );
        assert!(
            endpoint_from(&env_of(&[("XDG_RUNTIME_DIR", ""), ("HOME", "/home/u")])).is_none(),
            "an empty XDG_RUNTIME_DIR must refuse, not produce a relative path"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_application_support() {
        let e = endpoint_from(&env_of(&[("HOME", "/Users/u")])).expect("endpoint");
        assert_eq!(
            e.ipc_path,
            PathBuf::from("/Users/u/Library/Application Support/UniversalLink/core.sock")
        );
        assert_eq!(
            e.token_path(),
            PathBuf::from("/Users/u/Library/Application Support/UniversalLink/ipc-token")
        );
        assert!(endpoint_from(&env_of(&[])).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_uses_per_user_pipe_and_appdata() {
        let e = endpoint_from(&env_of(&[
            ("USERDOMAIN", "PC-IWAN"),
            ("USERNAME", "iwan"),
            ("APPDATA", r"C:\Users\iwan\AppData\Roaming"),
        ]))
        .expect("endpoint");
        // The domain is part of the name: a local account and a same-named
        // domain account do not share the same pipe.
        assert_eq!(
            e.ipc_path,
            PathBuf::from(r"\\.\pipe\universallink-core-PC-IWAN-iwan")
        );
        assert_eq!(
            e.token_path(),
            PathBuf::from(r"C:\Users\iwan\AppData\Roaming\UniversalLink\ipc-token")
        );
        // An indispensable variable absent or empty: refusal.
        assert!(
            endpoint_from(&env_of(&[("USERDOMAIN", "PC-IWAN"), ("USERNAME", "iwan")])).is_none()
        );
        assert!(
            endpoint_from(&env_of(&[
                ("USERDOMAIN", ""),
                ("USERNAME", "iwan"),
                ("APPDATA", r"C:\Users\iwan\AppData\Roaming"),
            ]))
            .is_none()
        );
    }
}
