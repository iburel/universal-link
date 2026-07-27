// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Per-OS surface construction. The real surfaces land one brick per platform:
//! Linux (KDE ServiceMenus for Dolphin + Nautilus scripts), Windows (the classic
//! `HKCU\…\shell` cascade + Send to), macOS (Quick Actions in
//! `~/Library/Services`). Each is family A — an artifact on disk or in the
//! registry that a normal process rewrites, whose command line starts our
//! `--send` helper.
//!
//! The family-B surfaces (the Windows 11 main menu's `IExplorerCommand` COM DLL,
//! a FinderSync appex) are deliberately out of scope: both require a SIGNED,
//! statically-registered artifact, and milestone 1 ships unsigned installers.
//! They will plug into the manager through the local channel's `targets` pull,
//! which is why that request already exists.
//!
//! Until a platform has a surface, [`create`] reports [`Unsupported`] and `main`
//! exits cleanly. The component is deliberately NOT registered in the Core's
//! supervisor while that is the case: a child that exits immediately would be
//! restarted forever (backing off to one launch a minute), so registration comes
//! per platform with its surface.

use crate::surface::{HelperCommand, MenuSurface};

/// No menu surface is available on this platform yet.
#[derive(Debug)]
pub struct Unsupported;

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no contextual-menu surface on this platform yet")
    }
}

impl std::error::Error for Unsupported {}

/// Builds the platform's menu surfaces. Every surface a platform has is returned
/// together: they render the same target list, and one failing does not silence
/// the others.
pub fn create(_helper: HelperCommand) -> Result<Vec<Box<dyn MenuSurface>>, Unsupported> {
    Err(Unsupported)
}
