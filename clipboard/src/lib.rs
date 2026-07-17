// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The clipboard backend component: the OS-agnostic orchestrator plus the seam
//! the per-OS backends plug into.
//!
//! A supervised component must (see `daemon/src/supervisor.rs`, "Contract of a
//! supervised component"): find the Core at `UNIVERSALLINK_IPC_PATH`, read its
//! spawn token from the first line of standard input, keep that standard input
//! open (its EOF means "stop"), and exit if it loses its IPC connection — the
//! spawn token is single-use, so a reconnection would fail; exiting lets the
//! supervisor restart it with a fresh token.
//!
//! Two seams meet in [`run`]:
//! - the **Core** side, over [`universallink_ipc_client`]: it announces local
//!   copies (`clipboard.updated`), serves inline pastes (`clipboard.get_data` →
//!   a provider channel), learns of remote copies (`clipboard.remote_updated`),
//!   and pulls remote bytes at paste time (`transactions.open` → a consumer
//!   channel). The protocol is frozen in `doc/core-api.md`.
//! - the **OS** side, over [`ClipboardBackend`] (downcalls: read/deliver/offer/
//!   release) and [`BackendEvent`] (upcalls: a local copy, a clear, a paste).
//!   Brick 1 ships the seam and the orchestrator; the real backends (X11,
//!   Windows, macOS) land per platform in later bricks.
//!
//! The orchestrator is OS-agnostic on purpose, so it is exercised against a
//! real Core (`tests/api/`) through a test double rather than a live desktop.

pub mod backend;
#[cfg(target_os = "linux")]
mod files;
#[cfg(target_os = "linux")]
mod fuse;
#[cfg(target_os = "macos")]
mod macos;
mod orchestrator;
pub mod os;
#[cfg(target_os = "linux")]
mod webdav;
#[cfg(target_os = "windows")]
mod windows;
// The OLE data object (destination side of the Windows files brick). Private:
// its only public entry, `build_files_data_object`, is used from `windows`, and
// its COM behavior is covered by the `#[cfg(test)]` unit tests inside the module.
#[cfg(target_os = "windows")]
mod windows_ole;
#[cfg(target_os = "linux")]
mod x11;

pub use backend::{
    BackendEvent, ClipboardBackend, FileFetcher, Format, LocalClip, RemoteClip, RemoteFile,
};
// The FUSE mount is a private module, but its mount + probe are re-exported on
// Linux so the native-only files integration test (tests/linux_files.rs, all
// `#[ignore]`d) can drive a real mount without the private module path.
#[cfg(target_os = "linux")]
pub use fuse::{FuseMount, fuse_available};
pub use orchestrator::{Outcome, run};
// The WebDAV fallback is likewise private, but its bare server + probe are
// re-exported on Linux so the loopback integration test (tests/linux_webdav.rs)
// can drive the HTTP surface with no gio/GVFS.
#[cfg(target_os = "linux")]
pub use webdav::{WebDavMount, WebDavServer, webdav_available};
