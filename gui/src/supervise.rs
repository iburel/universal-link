// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Bringing the Core along WITH the GUI, without privileges. The Core is a
//! PER-USER agent (keychain, account, received files all are): never a system
//! service. So at GUI startup we do two things:
//!
//! 1. `spawn_core`: launch it now if it isn't already running. Unconditional
//!    and safe — the Core holds a single-instance lock and exits with 0 if one
//!    is already running (see the `universallink-core` binary), so a redundant
//!    spawn does nothing. The Core is detached: it survives the GUI closing,
//!    which allows receiving a transfer with the window closed.
//! 2. `register_autostart`: register it so it restarts at each session login
//!    (macOS LaunchAgent / Windows HKCU Run key / Linux XDG autostart). The
//!    CURRENT session is already covered by the direct spawn; autostart takes
//!    over for SUBSEQUENT sessions. Rewritten at each launch (idempotent): if
//!    the app is moved, the path fixes itself.
//!
//! Nothing here requires privileges: everything is placed in the user's space.
//! The day a specific backend demands admin, we'll isolate THAT bit into a
//! small privileged helper — not the entire Core.

use std::path::{Path, PathBuf};

/// Name of the Core binary bundled alongside the GUI (Tauri `externalBin`
/// sidecar). Tauri strips the target-triple suffix at packaging time: at
/// runtime it is simply `universallink-core[.exe]`.
pub const CORE_BIN: &str = if cfg!(windows) {
    "universallink-core.exe"
} else {
    "universallink-core"
};

/// Label of the macOS LaunchAgent (= plist label + file name). Reuses the
/// bundle identifier. Windows/Linux name their entry differently.
#[cfg(target_os = "macos")]
const AUTOSTART_LABEL: &str = "org.universallink.core";

/// The bundled Core: alongside the GUI executable (the bundle places the
/// `externalBin` in the same folder as the main binary). `None` if we can't
/// resolve our own path.
pub fn bundled_core_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(CORE_BIN))
}

/// Launches the Core in the background. Non-blocking and non-fatal: if the
/// binary is missing (dev build without a bundle) or the spawn fails, the GUI
/// starts anyway and will display the connection state.
pub fn spawn_core(core_path: &Path) {
    if !core_path.exists() {
        eprintln!(
            "[universallink] Core not found alongside the GUI ({}): no spawn (dev build?)",
            core_path.display()
        );
        return;
    }
    // `mut` required only to set a creation flag on Windows.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = std::process::Command::new(core_path);
    // No console window flashing when the GUI launches the Core.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(_child) => eprintln!("[universallink] Core launched (or already running)"),
        Err(e) => eprintln!("[universallink] cannot spawn the Core: {e}"),
    }
}

/// Registers the Core at session startup (idempotent, non-fatal).
pub fn register_autostart(core_path: &Path) {
    if let Err(e) = register_autostart_inner(core_path) {
        eprintln!("[universallink] cannot register autostart: {e}");
    }
}

/// Contents of the macOS LaunchAgent. `RunAtLoad` launches the Core at session
/// login; `KeepAlive`/`SuccessfulExit=false` relaunches it if it CRASHES but
/// NOT if it exits cleanly (0) — which is what a redundant instance does when
/// it finds the lock already taken: so no restart loop. Pure function (tested).
#[cfg(any(test, target_os = "macos"))]
fn launch_agent_plist(label: &str, program: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{program}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
        program = xml_escape(&program.display().to_string()),
    )
}

/// XDG autostart entry (Linux, mostly for dev). `Terminal=false`: no terminal;
/// the entry is enabled by default. Pure function (tested).
#[cfg(any(test, target_os = "linux"))]
fn autostart_desktop_entry(program: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=UniversalLink Core\n\
         Exec={program}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        program = program.display(),
    )
}

/// Escapes the bare minimum for an XML text-node content (the plist).
#[cfg(any(test, target_os = "macos"))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn register_autostart_inner(core_path: &Path) -> std::io::Result<()> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME missing"))?;
    let dir = PathBuf::from(home).join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    // We don't `launchctl bootstrap` now: the current session is covered by
    // the direct spawn; launchd will load the agent (RunAtLoad + KeepAlive
    // supervision) at the next session login.
    std::fs::write(
        dir.join(format!("{AUTOSTART_LABEL}.plist")),
        launch_agent_plist(AUTOSTART_LABEL, core_path),
    )
}

#[cfg(target_os = "linux")]
fn register_autostart_inner(core_path: &Path) -> std::io::Result<()> {
    // ~/.config/autostart (XDG Autostart spec). XDG_CONFIG_HOME takes precedence.
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "neither XDG_CONFIG_HOME nor HOME")
        })?;
    let dir = base.join("autostart");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("universallink-core.desktop"),
        autostart_desktop_entry(core_path),
    )
}

#[cfg(windows)]
fn register_autostart_inner(core_path: &Path) -> std::io::Result<()> {
    // HKCU Run key (per-user, no privileges). We go through `reg.exe` to avoid
    // depending on any registry crate (nothing to compile/validate off
    // Windows). The data is the QUOTED path: at login, Windows re-parses the
    // line, and the quotes protect a path with spaces.
    let quoted = format!("\"{}\"", core_path.display());
    let status = std::process::Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "UniversalLink",
            "/t",
            "REG_SZ",
            "/d",
            &quoted,
            "/f",
        ])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "reg add failed (code {:?})",
            status.code()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_names_the_program_and_survives_crashes_only() {
        let plist = launch_agent_plist("org.universallink.core", Path::new("/Apps/UL.app/x/core"));
        assert!(plist.contains("<string>org.universallink.core</string>"));
        assert!(plist.contains("<string>/Apps/UL.app/x/core</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        // Conditional KeepAlive: we relaunch on crash, never on exit 0
        // (otherwise the redundant instance that exits with 0 would loop).
        assert!(plist.contains("<key>SuccessfulExit</key>"));
        assert!(plist.contains("<false/>"));
    }

    #[test]
    fn the_plist_escapes_xml_metacharacters_in_the_path() {
        // A path may contain & or <: the plist must remain valid XML.
        let plist = launch_agent_plist("l", Path::new("/a & b/<core>"));
        assert!(plist.contains("/a &amp; b/&lt;core&gt;"));
        assert!(!plist.contains("/a & b/<core>"));
    }

    #[test]
    fn the_desktop_entry_points_at_the_program() {
        let entry = autostart_desktop_entry(Path::new("/opt/ul/core"));
        assert!(entry.contains("Exec=/opt/ul/core"));
        assert!(entry.contains("Terminal=false"));
        assert!(entry.starts_with("[Desktop Entry]"));
    }

    #[test]
    fn the_core_binary_name_matches_the_platform() {
        if cfg!(windows) {
            assert_eq!(CORE_BIN, "universallink-core.exe");
        } else {
            assert_eq!(CORE_BIN, "universallink-core");
        }
    }
}
