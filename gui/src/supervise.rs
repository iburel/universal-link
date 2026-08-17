// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Bringing the Core along WITH the GUI, without privileges. The Core is a
//! PER-USER agent (keychain, account, received files all are): never a system
//! service. So at GUI startup we do two things:
//!
//! 1. `spawn_core`: launch it now if it isn't already running. Unconditional
//!    and safe — the Core holds a single-instance lock and exits with 0 if one
//!    is already running (see the `1device-core` binary), so a redundant
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
/// runtime it is simply `1device-core[.exe]`.
pub const CORE_BIN: &str = if cfg!(windows) {
    "1device-core.exe"
} else {
    "1device-core"
};

/// Label of the macOS LaunchAgent (= plist label + file name). Reuses the
/// bundle identifier. Windows/Linux name their entry differently.
#[cfg(target_os = "macos")]
const AUTOSTART_LABEL: &str = "org.onedevice.core";

/// The bundled Core: alongside the GUI executable (the bundle places the
/// `externalBin` in the same folder as the main binary). `None` if we can't
/// resolve our own path.
pub fn bundled_core_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(CORE_BIN))
}

/// What the tray should run to bring the GUI up. The tray runs from the Core's
/// durable copy and cannot otherwise find the GUI, so the GUI records this at
/// startup. `None` if we cannot resolve our own path.
///
/// - Linux: `$APPIMAGE` when launched from one (the loose file to re-run),
///   otherwise the executable (dev / native install).
/// - macOS: the `.app` bundle — opened with `open`, which activates an existing
///   instance instead of duplicating it — otherwise the executable.
/// - Windows: the executable.
pub fn launch_target() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    #[cfg(target_os = "linux")]
    {
        if let Some(appimage) = std::env::var_os("APPIMAGE") {
            return Some(PathBuf::from(appimage));
        }
        Some(exe)
    }
    #[cfg(target_os = "macos")]
    {
        Some(app_bundle_path(&exe).unwrap_or(exe))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Some(exe)
    }
}

/// The enclosing `.app` bundle of a macOS executable at
/// `Foo.app/Contents/MacOS/Foo`; `None` if the path is not shaped like that.
/// Pure function (tested on every platform).
#[cfg(any(test, target_os = "macos"))]
fn app_bundle_path(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?; // Contents/MacOS
    let contents = macos.parent()?; // Contents
    let bundle = contents.parent()?; // Foo.app
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app")
        .then(|| bundle.to_path_buf())
}

/// Records the GUI relaunch target (see [`launch_target`]) at `dest` for the
/// tray to read. Best-effort: on failure the tray's "Open" is simply a no-op.
pub fn record_launch_target(dest: &Path) {
    let Some(target) = launch_target() else {
        return;
    };
    if let Err(e) = std::fs::write(dest, target.to_string_lossy().as_bytes()) {
        eprintln!("[1device] cannot record the GUI launch path: {e}");
    }
}

/// The Core to actually spawn and register for autostart. On most platforms
/// this is just the bundled sidecar (`bundled`) — its location is durable
/// (macOS `.app` in /Applications, per-user NSIS install dir). On Linux
/// launched from an AppImage it is NOT: the executable lives in an EPHEMERAL
/// mount (`/tmp/.mount_*`) that vanishes when the AppImage exits, so an
/// autostart entry pointing there would be dead at the next login. There we
/// copy the Core — which has no GTK/webkit dependency and runs standalone —
/// into a stable per-user location and return that path instead.
///
/// Non-fatal: on any error we fall back to `bundled`. This session still runs
/// (spawned from the mount, which is alive right now); only cross-session
/// autostart is at risk.
#[cfg(target_os = "linux")]
pub fn stabilize_core_path(bundled: &Path) -> PathBuf {
    // Only INSIDE an AppImage is the bundled path ephemeral. Outside one (dev
    // run, or a native package installed to a stable prefix) it is already
    // durable — leave it be.
    if std::env::var_os("APPIMAGE").is_none() {
        return bundled.to_path_buf();
    }
    let staged = data_home().and_then(|data| {
        let core = stage_core_copy(bundled, &data)?;
        // Bring the sidecars (tray, clipboard backend) alongside the durable
        // Core: the Core's supervisor looks for them next to its OWN executable,
        // which is now this copy — not the AppImage mount. Best-effort — a Core
        // without its tray or clipboard still works.
        stage_sidecars(bundled, &data);
        Ok(core)
    });
    match staged {
        Ok(stable) => stable,
        Err(e) => {
            eprintln!(
                "[1device] cannot stage a durable Core copy ({e}); \
                 autostart may not survive logout — using {}",
                bundled.display()
            );
            bundled.to_path_buf()
        }
    }
}

/// Non-Linux: the bundled path is already durable — nothing to stabilize.
#[cfg(not(target_os = "linux"))]
pub fn stabilize_core_path(bundled: &Path) -> PathBuf {
    bundled.to_path_buf()
}

/// Is `entry` the bundle directory itself, or something inside it?
///
/// Compared as a path prefix and not as a substring, so a NEIGHBOURING mount —
/// `/tmp/.mount_Univer0002` while we are `/tmp/.mount_Univer0001` — is not
/// mistaken for ours.
#[cfg(target_os = "linux")]
fn inside(entry: &str, appdir: &str) -> bool {
    entry == appdir || entry.starts_with(&format!("{appdir}/"))
}

/// What to change in the environment of a child that must NOT run against the
/// AppImage's bundled libraries: `(name, Some(value))` to set, `(name, None)`
/// to remove. Only the entries that need changing are returned.
///
/// linuxdeploy's `AppRun` points a dozen variables at `$APPDIR` so the bundled
/// GTK finds its own everything — `LD_LIBRARY_PATH`, `GIO_EXTRA_MODULES`,
/// `GDK_PIXBUF_MODULE_FILE`, `GTK_PATH`, `XDG_DATA_DIRS`, `PATH`… A child that
/// inherits those loads the bundle's libraries, and the Core we spawn is the
/// staged copy: it lives OUTSIDE the mount and links against the host's.
///
/// Measured, not theorised: on a Debian 13 desktop the tray dlopens the host's
/// `libayatana-appindicator3.so.1`, which pulls `libayatana-ido3` and wants
/// `g_once_init_leave_pointer` — a glib 2.80 symbol the bundle's glib (from the
/// 22.04 build host) does not carry. The dlopen fails, the tray panics, and the
/// supervisor restarts it about once a second for the rest of the session. The
/// same mismatch stops the host's gvfs gio module from loading.
///
/// The rule is about VALUES, not names: drop every entry that lives under
/// `$APPDIR`, keep the host's, remove a variable that had nothing else in it.
/// So there is no list of variable names to keep in step with linuxdeploy's
/// hooks, and a variable naming no path — `GTK_THEME`, `GDK_BACKEND` — is left
/// exactly as it is. Empty entries go too: they are the seam of the hook's own
/// concatenation, and an empty entry in `PATH` means "the current directory".
///
/// This applies whether the Core runs from the staged copy or (staging having
/// failed) from the mount itself: the Core links against no bundled library
/// either way, and neither do the components it spawns.
#[cfg(target_os = "linux")]
fn env_without_bundle_paths<I>(vars: I, appdir: &str) -> Vec<(String, Option<String>)>
where
    I: IntoIterator<Item = (String, String)>,
{
    // The runtime's own markers: they describe a bundle the child is not in.
    // Nothing outside this module reads them.
    const MARKERS: [&str; 4] = ["APPDIR", "APPIMAGE", "ARGV0", "OWD"];

    let appdir = appdir.trim_end_matches('/');
    if appdir.is_empty() {
        return Vec::new(); // not inside an AppImage: nothing to undo
    }
    let mut changes = Vec::new();
    for (name, value) in vars {
        if MARKERS.contains(&name.as_str()) {
            changes.push((name, None));
            continue;
        }
        if !value.contains(appdir) {
            continue; // the bundle never touched this one
        }
        let kept: Vec<&str> = value
            .split(':')
            .filter(|e| !e.is_empty() && !inside(e, appdir))
            .collect();
        if kept.is_empty() {
            changes.push((name, None));
        } else {
            let joined = kept.join(":");
            if joined != value {
                changes.push((name, Some(joined)));
            }
        }
    }
    changes
}

/// Applies [`env_without_bundle_paths`] to `cmd`. Split from the computation so
/// a test can read the wiring back off the command (`Command::get_envs`) instead
/// of mutating the process environment to observe it.
#[cfg(target_os = "linux")]
fn scrub_bundle_env<I>(cmd: &mut std::process::Command, vars: I, appdir: &str)
where
    I: IntoIterator<Item = (String, String)>,
{
    for (name, value) in env_without_bundle_paths(vars, appdir) {
        match value {
            Some(v) => cmd.env(name, v),
            None => cmd.env_remove(name),
        };
    }
}

/// Launches the Core in the background. Non-blocking and non-fatal: if the
/// binary is missing (dev build without a bundle) or the spawn fails, the GUI
/// starts anyway and will display the connection state.
pub fn spawn_core(core_path: &Path) {
    if !core_path.exists() {
        eprintln!(
            "[1device] Core not found alongside the GUI ({}): no spawn (dev build?)",
            core_path.display()
        );
        return;
    }
    // `mut`: `spawn` takes `&mut self`, and both blocks below configure `cmd`.
    let mut cmd = std::process::Command::new(core_path);
    // Inside an AppImage, hand the child an environment scrubbed of the
    // bundle's library paths — see `env_without_bundle_paths`.
    #[cfg(target_os = "linux")]
    if let Some(appdir) = std::env::var("APPDIR").ok().filter(|d| !d.is_empty()) {
        scrub_bundle_env(&mut cmd, std::env::vars(), &appdir);
    }
    // No console window flashing when the GUI launches the Core.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(_child) => eprintln!("[1device] Core launched (or already running)"),
        Err(e) => eprintln!("[1device] cannot spawn the Core: {e}"),
    }
}

/// Registers the Core at session startup (idempotent, non-fatal).
pub fn register_autostart(core_path: &Path) {
    if let Err(e) = register_autostart_inner(core_path) {
        eprintln!("[1device] cannot register autostart: {e}");
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
         Name=1Device Core\n\
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
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "neither XDG_CONFIG_HOME nor HOME",
            )
        })?;
    let dir = base.join("autostart");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("1device-core.desktop"),
        autostart_desktop_entry(core_path),
    )
}

/// `$XDG_DATA_HOME`, else `~/.local/share` — the same precedence the autostart
/// entry above uses for `$XDG_CONFIG_HOME`. This is where the durable Core copy
/// lives when we run from an AppImage.
#[cfg(target_os = "linux")]
fn data_home() -> std::io::Result<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "neither XDG_DATA_HOME nor HOME",
            )
        })
}

/// Where the durable Core copy lives under a given data dir. Pure (tested).
#[cfg(target_os = "linux")]
fn staged_core_dest(data_home: &Path) -> PathBuf {
    data_home.join("1device").join(CORE_BIN)
}

/// Copies the Core into `<data_home>/1device/` and returns its path.
/// `data_home` is passed in (not read from the env) so the mechanics are
/// testable deterministically.
#[cfg(target_os = "linux")]
fn stage_core_copy(src: &Path, data_home: &Path) -> std::io::Result<PathBuf> {
    let dest = staged_core_dest(data_home);
    stage_copy(src, &dest)?;
    Ok(dest)
}

/// The sidecars the Core's supervisor looks up next to its OWN executable
/// (`official_components`). Inside an AppImage they sit next to the bundled Core
/// on the ephemeral mount; the durable copy must carry them too, or the
/// supervisor won't find them once we run from the copy. This list grows as
/// components are added; nothing cross-checks it against `official_components`, and
/// a sidecar missing from it is simply never launched on a real Linux install.
#[cfg(target_os = "linux")]
const STAGED_SIDECARS: &[&str] = &[
    "1device-tray",
    "1device-clipboard",
    "1device-menu",
    "1device-sync",
    "1device-input",
];

/// Copies each sidecar next to the durable Core (best-effort). A sidecar absent
/// from this build is skipped; a copy failure is logged but not fatal — a Core
/// without its tray or clipboard still runs.
#[cfg(target_os = "linux")]
fn stage_sidecars(bundled_core: &Path, data_home: &Path) {
    let Some(dir) = bundled_core.parent() else {
        return;
    };
    for bin in STAGED_SIDECARS {
        let src = dir.join(bin);
        if !src.exists() {
            continue; // not in this build
        }
        let dest = data_home.join("1device").join(bin);
        if let Err(e) = stage_copy(&src, &dest) {
            eprintln!("[1device] cannot stage the {bin} copy ({e})");
        }
    }
}

/// Copies `src` onto `dest` via a temp file then an atomic `rename(2)`: unlike
/// copying in place, this does NOT fail with `ETXTBSY` when `dest` is a binary
/// currently running from a previous session — the running process keeps its
/// old inode, the new file takes over the path. Creates `dest`'s parent.
#[cfg(target_os = "linux")]
fn stage_copy(src: &Path, dest: &Path) -> std::io::Result<()> {
    let dir = dest
        .parent()
        .expect("staged destination always has a parent");
    std::fs::create_dir_all(dir)?;
    let name = dest
        .file_name()
        .expect("staged destination has a file name")
        .to_string_lossy();
    let tmp = dir.join(format!("{name}.new"));
    std::fs::copy(src, &tmp)?;
    set_executable(&tmp)?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Marks a freshly written file executable (0o755). `std::fs::copy` already
/// carries the source mode over, but we set it explicitly so the durable copy
/// is runnable regardless of the source's bits.
#[cfg(target_os = "linux")]
fn set_executable(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(p)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(p, perms)
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
            "1Device",
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
        let plist = launch_agent_plist("org.onedevice.core", Path::new("/Apps/UL.app/x/core"));
        assert!(plist.contains("<string>org.onedevice.core</string>"));
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
    fn recording_writes_a_non_empty_launch_target() {
        // In the test process `launch_target` resolves the current executable,
        // so a non-empty path is written for the tray to read back.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dest = tmp.path().join("gui-launch");
        record_launch_target(&dest);
        let recorded = std::fs::read_to_string(&dest).expect("recorded launch target");
        assert!(!recorded.trim().is_empty());
    }

    #[test]
    fn app_bundle_path_ascends_to_the_dot_app() {
        let exe = Path::new("/Applications/1Device.app/Contents/MacOS/1Device");
        assert_eq!(
            app_bundle_path(exe),
            Some(PathBuf::from("/Applications/1Device.app"))
        );
        // Not inside a bundle (a bare executable): None, so the caller falls
        // back to the executable itself.
        assert_eq!(
            app_bundle_path(Path::new("/usr/local/bin/1device-gui")),
            None
        );
    }

    #[test]
    fn the_desktop_entry_points_at_the_program() {
        let entry = autostart_desktop_entry(Path::new("/opt/1device/core"));
        assert!(entry.contains("Exec=/opt/1device/core"));
        assert!(entry.contains("Terminal=false"));
        assert!(entry.starts_with("[Desktop Entry]"));
    }

    #[test]
    fn the_core_binary_name_matches_the_platform() {
        if cfg!(windows) {
            assert_eq!(CORE_BIN, "1device-core.exe");
        } else {
            assert_eq!(CORE_BIN, "1device-core");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn staged_dest_lives_under_the_data_home_namespace() {
        let dest = staged_core_dest(Path::new("/home/u/.local/share"));
        assert_eq!(
            dest,
            Path::new("/home/u/.local/share/1device").join(CORE_BIN)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn staging_copies_the_core_and_marks_it_executable() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("bundled-core");
        std::fs::write(&src, b"#!/bin/sh\necho core\n").expect("write src");

        let data_home = tmp.path().join("data");
        let dest = stage_core_copy(&src, &data_home).expect("stage");

        assert_eq!(dest, staged_core_dest(&data_home));
        assert_eq!(
            std::fs::read(&dest).expect("read dest"),
            b"#!/bin/sh\necho core\n"
        );
        let mode = std::fs::metadata(&dest).expect("meta").permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "the durable copy must be executable");
        // No temp left behind.
        assert!(
            !data_home
                .join("1device")
                .join(format!("{CORE_BIN}.new"))
                .exists()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn staging_a_sibling_copies_it_and_marks_it_executable() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("1device-tray");
        std::fs::write(&src, b"tray").expect("write src");

        let dest = tmp.path().join("data").join("1device").join("1device-tray");
        stage_copy(&src, &dest).expect("stage");

        assert_eq!(std::fs::read(&dest).expect("read"), b"tray");
        let mode = std::fs::metadata(&dest).expect("meta").permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "the sibling copy must be executable");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn staging_sidecars_brings_every_supervised_component_next_to_the_core() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Mimic the bundled layout: the Core and its sidecars side by side, as
        // on the AppImage mount.
        let mount = tmp.path().join("mount");
        std::fs::create_dir_all(&mount).expect("mount dir");
        let core = mount.join(CORE_BIN);
        std::fs::write(&core, b"core").expect("core");
        for bin in STAGED_SIDECARS {
            std::fs::write(mount.join(bin), bin.as_bytes()).expect("sidecar");
        }

        let data_home = tmp.path().join("data");
        stage_sidecars(&core, &data_home);

        // Every declared sidecar lands next to the durable Core.
        for bin in STAGED_SIDECARS {
            let dest = data_home.join("1device").join(bin);
            assert!(dest.exists(), "{bin} must be staged next to the Core");
            assert_eq!(std::fs::read(&dest).expect("read"), bin.as_bytes());
        }
        // And the list itself names every component the Core's supervisor launches
        // on Linux. The loop above cannot catch a name being DROPPED from it, which
        // is the failure that matters: a sidecar bundled in the AppImage but not
        // staged is never found next to the durable Core, so it silently never runs
        // — logged at INFO as "component absent" and nothing else.
        for expected in [
            "1device-tray",
            "1device-clipboard",
            "1device-menu",
            "1device-sync",
            "1device-input",
        ] {
            assert!(
                STAGED_SIDECARS.contains(&expected),
                "{expected} is launched by the supervisor but never staged"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn staging_sidecars_skips_the_ones_absent_from_the_build() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mount = tmp.path().join("mount");
        std::fs::create_dir_all(&mount).expect("mount dir");
        let core = mount.join(CORE_BIN);
        std::fs::write(&core, b"core").expect("core");
        // Only the tray is present; the clipboard is missing from this build.
        std::fs::write(mount.join("1device-tray"), b"tray").expect("tray");

        let data_home = tmp.path().join("data");
        stage_sidecars(&core, &data_home); // must not panic on the absent one

        assert!(data_home.join("1device").join("1device-tray").exists());
        assert!(
            !data_home.join("1device").join("1device-clipboard").exists(),
            "an absent sidecar is skipped, not fabricated"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn staging_overwrites_a_previous_copy_idempotently() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_home = tmp.path().join("data");

        let old = tmp.path().join("old-core");
        std::fs::write(&old, b"old").expect("write old");
        let dest1 = stage_core_copy(&old, &data_home).expect("stage old");

        // A second staging (AppImage updated between launches) replaces it.
        let new = tmp.path().join("new-core");
        std::fs::write(&new, b"new-and-longer").expect("write new");
        let dest2 = stage_core_copy(&new, &data_home).expect("stage new");

        assert_eq!(dest1, dest2);
        assert_eq!(std::fs::read(&dest2).expect("read"), b"new-and-longer");
    }

    /// The environment linuxdeploy's `AppRun` actually hands us, copied off a
    /// running 0.5.0 AppImage on a Debian 13 desktop, with the mount point
    /// replaced by `APPDIR`. Values verbatim, trailing slashes and empty
    /// entries included — those are what the rule has to survive.
    #[cfg(target_os = "linux")]
    fn measured_appimage_env(appdir: &str) -> Vec<(String, String)> {
        [
            ("APPDIR", appdir.to_string()),
            ("APPIMAGE", "/home/iwan/Applications/1Device_0.5.0_amd64.AppImage".into()),
            ("ARGV0", "/home/iwan/Applications/1Device_0.5.0_amd64.AppImage".into()),
            ("OWD", "/home/iwan".into()),
            ("LD_LIBRARY_PATH", format!("{appdir}/usr/lib/:{appdir}/usr/lib/x86_64-linux-gnu/:{appdir}/lib64/:")),
            ("PATH", format!("{appdir}/usr/bin/:{appdir}/usr/sbin/:{appdir}/bin/:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin")),
            ("XDG_DATA_DIRS", format!("{appdir}/usr/share/:{appdir}/usr/share:/usr/share:")),
            ("GTK_PATH", format!("{appdir}//usr/lib/x86_64-linux-gnu/gtk-3.0:/usr/lib64/gtk-3.0:/usr/lib/x86_64-linux-gnu/gtk-3.0")),
            ("GIO_EXTRA_MODULES", format!("{appdir}/usr/lib/x86_64-linux-gnu/gio/modules")),
            ("GDK_PIXBUF_MODULE_FILE", format!("{appdir}//usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache")),
            ("GSETTINGS_SCHEMA_DIR", format!("{appdir}/usr/share/glib-2.0/schemas/:{appdir}//usr/share/glib-2.0/schemas")),
            ("GTK_DATA_PREFIX", appdir.to_string()),
            ("GTK_EXE_PREFIX", format!("{appdir}//usr")),
            // Named no path: the rule must not reinterpret these. `GTK_THEME`
            // and `DISPLAY` even LOOK like colon lists.
            ("GTK_THEME", "Adwaita:light".into()),
            ("GDK_BACKEND", "x11".into()),
            ("DISPLAY", ":0".into()),
            ("HOME", "/home/iwan".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_bundle_paths_go_and_the_host_paths_stay() {
        let appdir = "/tmp/.mount_UniverLEHiHn";
        let changes: std::collections::HashMap<String, Option<String>> =
            env_without_bundle_paths(measured_appimage_env(appdir), appdir)
                .into_iter()
                .collect();

        // Mixed variables keep the host's entries, in order, and lose the
        // bundle's — plus the empty entry the hook's concatenation left behind
        // (in `PATH` an empty entry means "the current directory").
        assert_eq!(
            changes["PATH"].as_deref(),
            Some("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin")
        );
        assert_eq!(changes["XDG_DATA_DIRS"].as_deref(), Some("/usr/share"));
        assert_eq!(
            changes["GTK_PATH"].as_deref(),
            Some("/usr/lib64/gtk-3.0:/usr/lib/x86_64-linux-gnu/gtk-3.0")
        );

        // A variable that pointed at nothing but the bundle is removed, not
        // emptied: an empty `LD_LIBRARY_PATH` is not the same as none.
        for gone in [
            "LD_LIBRARY_PATH",
            "GIO_EXTRA_MODULES",
            "GDK_PIXBUF_MODULE_FILE",
            "GSETTINGS_SCHEMA_DIR",
            "GTK_DATA_PREFIX",
            "GTK_EXE_PREFIX",
            // The runtime's markers describe a bundle the child is not in.
            "APPDIR",
            "APPIMAGE",
            "ARGV0",
            "OWD",
        ] {
            assert_eq!(changes.get(gone), Some(&None), "{gone} must be removed");
        }

        // Untouched: not returned at all, so the child inherits them as they are.
        for kept in ["GTK_THEME", "GDK_BACKEND", "DISPLAY", "HOME"] {
            assert!(
                !changes.contains_key(kept),
                "{kept} names no bundle path and must be left alone"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_command_carries_the_removals_and_the_rewrites() {
        // The computation above is only half of it: a variable has to be
        // REMOVED from the child's environment, not set to the empty string,
        // and the command is where that distinction is made. Read back off the
        // command itself, so the process environment is never touched.
        let appdir = "/tmp/.mount_UniverLEHiHn";
        let mut cmd = std::process::Command::new("/bin/true");
        scrub_bundle_env(&mut cmd, measured_appimage_env(appdir), appdir);

        let wired: std::collections::HashMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();

        assert_eq!(wired["LD_LIBRARY_PATH"], None, "removed, not emptied");
        assert_eq!(wired["APPDIR"], None);
        assert_eq!(
            wired["XDG_DATA_DIRS"].as_deref(),
            Some("/usr/share"),
            "rewritten to the host's entries"
        );
        // What was left alone is absent from the command: the child inherits it.
        assert!(!wired.contains_key("GTK_THEME"));
        assert!(!wired.contains_key("HOME"));
    }

    /// The whole path, for real: `spawn_core` launches a child that reports the
    /// environment it was actually given.
    ///
    /// The two tests above stop at the `Command`; this one is the only place
    /// that proves the scrub is wired into the spawn at all — and it is worth
    /// the awkwardness, because the bug it guards against was a spawn that
    /// silently passed everything on.
    ///
    /// `set_var` is process-wide, hence `unsafe` in edition 2024. It is sound
    /// here: `cargo-nextest`, which is what CI runs, gives every test its own
    /// process, and no other test in this crate reads these variables (only
    /// `APPIMAGE`, which this test deliberately leaves alone).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_spawned_child_really_gets_the_scrubbed_environment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let core = tmp.path().join("1device-core");
        let reported = tmp.path().join("child-env.txt");
        std::fs::write(&core, format!("#!/bin/sh\nenv > {}\n", reported.display()))
            .expect("write the stand-in Core");
        set_executable(&core).expect("chmod +x");

        let appdir = "/tmp/.mount_UniverTEST";
        unsafe {
            std::env::set_var("APPDIR", appdir);
            std::env::set_var("LD_LIBRARY_PATH", format!("{appdir}/usr/lib:"));
            std::env::set_var("XDG_DATA_DIRS", format!("{appdir}/usr/share:/usr/share:"));
        }

        spawn_core(&core);

        // The child is a shell: give it a moment, but do not sleep a fixed
        // second for nothing.
        let mut dumped = String::new();
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&reported)
                && s.contains('\n')
            {
                dumped = s;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !dumped.is_empty(),
            "the stand-in Core never reported its environment"
        );

        let lines: Vec<&str> = dumped.lines().collect();
        assert!(
            !lines.iter().any(|l| l.starts_with("APPDIR=")),
            "APPDIR reached the child: {dumped}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("LD_LIBRARY_PATH=")),
            "the bundle's library path reached the child: {dumped}"
        );
        assert!(
            lines.contains(&"XDG_DATA_DIRS=/usr/share"),
            "the host's data dir should survive alone: {dumped}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_neighbouring_mount_is_not_taken_for_ours() {
        // Two AppImages running at once: ours, and someone else's whose mount
        // point starts with the same characters. A substring test would strip
        // the other one's paths out of the child's environment.
        let ours = "/tmp/.mount_Univer0001";
        let vars = vec![(
            "LD_LIBRARY_PATH".to_string(),
            format!("{ours}/usr/lib:/tmp/.mount_Univer0002/usr/lib:/usr/lib"),
        )];
        let changes = env_without_bundle_paths(vars, ours);
        assert_eq!(
            changes,
            vec![(
                "LD_LIBRARY_PATH".to_string(),
                Some("/tmp/.mount_Univer0002/usr/lib:/usr/lib".to_string())
            )]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn outside_an_appimage_nothing_changes() {
        // No `APPDIR` to strip means no bundle: not even the markers are
        // touched, so a dev run or a native package spawns the Core with the
        // environment it was given.
        assert!(env_without_bundle_paths(measured_appimage_env("/x"), "").is_empty());
        assert!(env_without_bundle_paths(measured_appimage_env("/x"), "/").is_empty());
    }
}
