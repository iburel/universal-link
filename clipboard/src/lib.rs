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
//!
//! # Confidentiality (`sensitive`)
//!
//! A clip is `sensitive` when the source OS carries a confidentiality marker; the
//! flag flows end to end (both directions) and the orchestrator omits the inline
//! size hint for it ([`orchestrator`], enforced once for every backend). Each
//! backend maps `sensitive` to its platform's marker convention, in BOTH
//! directions — detect it on a foreign copy, re-apply it when promising a remote
//! clip (doc/core-api.md, "re-applies the OS confidentiality markers"):
//! - **X11**: KDE's `x-kde-passwordManagerHint` (answered `"secret"`).
//! - **Windows**: `ExcludeClipboardContentFromMonitorProcessing` (detected +
//!   set), plus `CanIncludeInClipboardHistory` / `CanUploadToCloudClipboard`
//!   (DWORD `0`) on an offer — defense in depth (out of monitors, Win+V, cloud).
//! - **macOS**: `org.nspasteboard.ConcealedType` (nspasteboard.org — macOS has no
//!   first-party concealment API).
//!
//! ## Residual: a speculative reader
//!
//! These markers only bind a COOPERATING reader (a clipboard manager / indexer /
//! thumbnailer that honors them and skips the item). A reader that instead reads
//! our promised offer WITHOUT a real user paste — some clipboard managers eager-
//! read every target on an ownership change; a file indexer walks a mounted tree —
//! triggers the same on-demand pull a paste would, so a `sensitive` clip's bytes
//! can be pulled over the network with no paste. There is no portable way to tell
//! a real paste from a speculative read, so this residual is documented and NOT
//! worked around by refusing to offer (which would break legitimate pastes); the
//! markers are the mitigation cooperating readers honor. The Linux FUSE files path
//! is the most exposed (an indexer walking the mount), and a `sensitive` files
//! clip there is already FUSE-only (never the weaker loopback WebDAV).

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
