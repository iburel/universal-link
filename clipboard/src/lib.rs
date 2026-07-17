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
mod orchestrator;
pub mod os;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
mod x11;

pub use backend::{BackendEvent, ClipboardBackend, Format, LocalClip, RemoteClip, RemoteFile};
pub use orchestrator::{Outcome, run};
