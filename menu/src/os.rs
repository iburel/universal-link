// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Per-OS surface construction.
//!
//! Linux (brick 2) has a KDE ServiceMenu for Dolphin and Nautilus scripts; Windows
//! (brick 3) has the classic `HKCU\…\shell` cascade — twice, for files and for
//! folders — and one "Send to" shortcut per device; macOS (brick 4) has one
//! Automator workflow per device in `~/Library/Services`, which Finder shows among
//! its Quick Actions and services. Each is family A — an artifact on disk or in the
//! registry that a normal process rewrites, whose command line starts our `--send`
//! helper.
//!
//! The family-B surfaces (the Windows 11 main menu's `IExplorerCommand` COM DLL,
//! a FinderSync appex) are deliberately out of scope: both require a SIGNED,
//! statically-registered artifact, and milestone 1 ships unsigned installers.
//! They will plug into the manager through the local channel's `targets` pull,
//! which is why that request already exists.
//!
//! Where [`create`] reports [`Unsupported`], `main` exits 0 at once: there is
//! nothing this process could do, and nothing for the supervisor to restart. The
//! component is therefore registered in `official_components()` only on the
//! platforms that have a surface — a child that exits immediately would otherwise
//! be relaunched for ever, backing off to one launch a minute.

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

use crate::surface::{HelperCommand, MenuSurface};

/// No menu surface could be built here. Not a failure: the caller exits 0.
///
/// It carries its reason as text because nothing matches on it — it reaches the
/// supervisor's log and stops there.
#[derive(Debug)]
pub struct Unsupported(String);

impl Unsupported {
    pub(crate) fn new(reason: impl Into<String>) -> Unsupported {
        Unsupported(reason.into())
    }
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unsupported {}

/// Builds the platform's menu surfaces. Every surface a platform has is returned
/// together: they render the same target list, and one failing does not silence
/// the others.
pub fn create(helper: HelperCommand) -> Result<Vec<Box<dyn MenuSurface>>, Unsupported> {
    // A menu entry is a command line kept on disk: a path that cannot be written as
    // text cannot be named in one, and writing it lossily would bake in a command
    // line pointing at a file that does not exist — a menu whose every entry fails
    // silently. Refused up front instead, once, rather than by each surface on every
    // apply.
    if helper.program.to_str().is_none() {
        return Err(Unsupported::new(format!(
            "our own path is not valid UTF-8, so no menu entry can name it: {}",
            helper.program.display()
        )));
    }
    #[cfg(target_os = "linux")]
    {
        let data_home = linux::data_home().ok_or_else(|| {
            Unsupported::new("neither XDG_DATA_HOME nor HOME is set: nowhere to write a menu entry")
        })?;
        Ok(linux::surfaces(&data_home, helper))
    }
    #[cfg(target_os = "macos")]
    {
        let services = macos::services_dir().ok_or_else(|| {
            Unsupported::new("HOME is not set: nowhere to write a workflow the system would read")
        })?;
        Ok(macos::surfaces(&services, helper))
    }
    #[cfg(target_os = "windows")]
    {
        // Nothing to resolve up front: the registry root is fixed, and the Send to
        // folder is asked for by the surface itself.
        Ok(windows::surfaces(helper))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = helper;
        Err(Unsupported::new(
            "no contextual-menu surface on this platform yet",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn helper(program: std::path::PathBuf) -> HelperCommand {
        HelperCommand {
            program,
            extra_args: vec![],
        }
    }

    /// The wiring, not the paths: this reads the real environment, so it only
    /// constructs the surfaces (it never applies them, which would rewrite the
    /// developer's own menus). What it pins is that both Linux surfaces are
    /// reachable from `main` — the surfaces themselves are tested against a
    /// temporary data home.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_offers_both_surfaces() {
        let surfaces =
            create(helper(std::path::PathBuf::from("/opt/1device-menu"))).expect("surfaces");
        let names: Vec<&str> = surfaces.iter().map(|s| s.name()).collect();
        assert_eq!(names, ["kde-servicemenu", "nautilus-scripts"]);
    }

    /// Same, for Windows: three surfaces, and the Send to one is only there when
    /// its folder resolves — which on any real session it does.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_offers_both_cascades_and_send_to() {
        let surfaces = create(helper(std::path::PathBuf::from(
            r"C:\Program Files\UL\1device-menu.exe",
        )))
        .expect("surfaces");
        let names: Vec<&str> = surfaces.iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            [
                "windows-cascade-files",
                "windows-cascade-folders",
                "windows-send-to"
            ]
        );
    }

    /// And macOS: one surface, because a single `NSSendFileTypes` of `public.item`
    /// covers files and folders alike (measured — see [`macos`]), where Windows needs
    /// a cascade for each.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_offers_the_services_surface() {
        let surfaces = create(helper(std::path::PathBuf::from(
            "/Applications/1Device.app/1device-menu",
        )))
        .expect("surfaces");
        let names: Vec<&str> = surfaces.iter().map(|s| s.name()).collect();
        assert_eq!(names, ["macos-services"]);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    #[test]
    fn a_platform_without_a_surface_says_so() {
        assert!(create(helper(std::path::PathBuf::from("/opt/menu"))).is_err());
    }

    /// A menu entry is a line of text: our own path has to be expressible in one.
    /// Refused here rather than written lossily, which would bake in a command line
    /// naming a file that does not exist — every click failing, silently.
    #[cfg(unix)]
    #[test]
    fn a_program_path_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;

        let bad = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"/opt/caf\xe9/menu"));
        assert!(create(helper(bad)).is_err());
    }
}
