// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The contextual-menu component: the OS-agnostic manager plus the seam the
//! per-OS surfaces plug into.
//!
//! A supervised component must (see `daemon/src/supervisor.rs`, "Contract of a
//! supervised component"): find the Core at `UNIVERSALLINK_IPC_PATH`, read its
//! spawn token from the first line of standard input, keep that standard input
//! open (its EOF means "stop"), and exit if it loses its IPC connection — the
//! spawn token is single-use, so a reconnection would fail; exiting lets the
//! supervisor restart it with a fresh token.
//!
//! # Three seams meet in [`run`]
//!
//! - the **Core**, over [`universallink_ipc_client`]: it mirrors the directory
//!   (`devices.list` + the `devices` topic), follows the session (`session.status`
//!   + the `session` topic), and sends on a click (`files.send`).
//! - the **OS**, over [`MenuSurface`]: downcalls only. Rendering a target list is
//!   the whole contract, because a click never returns this way.
//! - the **couriers**, over [`channel`]: a click is a fresh process the shell
//!   started from an on-disk command line, and it reaches us on a private local
//!   socket. It must never hold Core credentials of its own — see the module's
//!   header.
//!
//! # What the menu offers, and when
//!
//! Fail-closed, per doc/architecture.md: an entry exists only when the system is
//! functional and targets exist. The rules live in [`targets::Directory::targets`]
//! — online, attested, not us, not the phone, and only while the Core is actually
//! connected to the server. No manager means no entry either: the surfaces are
//! emptied at startup and at graceful shutdown.
//!
//! The accepted residual (decision, 2026-07-27): a manager that *crashes* leaves
//! its artifacts behind until the supervisor restarts it. A click on one then
//! finds no channel and fails silently, which is why the helper needs no error
//! dialog.
//!
//! # Bricks
//!
//! Brick 1 was the manager, the channel and the seam; brick 2 is the Linux
//! surfaces ([`os::linux`]: KDE ServiceMenus for Dolphin, Nautilus scripts). Both
//! are frozen by `tests/api/` against a real Core — the Linux part down to running
//! the courier binary from the command line the artifacts actually carry. Bricks 3
//! and 4 add the Windows (`HKCU\…\shell` cascade + Send to) and macOS (Quick
//! Actions) surfaces.
//!
//! Brick 2 is also where the component stopped being inert, so the four
//! packaging obligations are now DONE for Linux and each remaining brick has to
//! extend them, or it ships a surface nothing launches:
//! 1. the tuple in `official_components()` (`daemon/src/supervisor.rs`) — add the
//!    `#[cfg(target_os = …)]` push for the new platform;
//! 2. three things in `.github/workflows/release.yml` — the `cargo build` step,
//!    the `cp` into `gui/binaries` with the target-triple suffix, and the
//!    `externalBin` entry in the `--config` JSON (NOT in `gui/tauri.conf.json`:
//!    tauri validates sidecar existence at compile time and would break every
//!    plain `cargo build`). All three are in place and platform-agnostic, so a
//!    new surface needs nothing here;
//! 3. the binary in `STAGED_SIDECARS` (`gui/src/supervise.rs`) — Linux only, and
//!    done: the GUI copies the Core out of the AppImage mount and the supervisor
//!    then looks for siblings next to that copy, so a sidecar missing from this
//!    list is never launched on a real Linux install;
//! 4. any build-time system library, in BOTH `ci.yml` and `release.yml`. Linux
//!    needed none — its surfaces are plain files under `$XDG_DATA_HOME`.
//!
//! Nothing cross-checks those lists — a component absent from an installer is
//! logged at INFO and silently does nothing.
//!
//! And one obligation that stays theirs alone, because escaping has no
//! format-independent answer: **quote the device NAME for the artifact's own
//! syntax**. The id baked into the command line is ours (`d_<hex>`, minted by the
//! server), but the label comes from `devices.rename` — a PC renamed
//! `"; rm -rf ~; #` must not become a shell injection in a Nautilus script, a
//! broken `Exec=` line in a `.desktop` file, or a mangled registry value. The
//! label is deliberately NOT sanitized centrally: a legitimate name contains
//! apostrophes and spaces, and mangling it would corrupt what the user sees.
//! Brick 2 shows the two answers a surface can give — escaping, where the format
//! has an escape (Dolphin's `Name=`), and sanitizing, where it has none because
//! the label IS a file name (Nautilus).

pub mod applier;
pub mod channel;
mod orchestrator;
pub mod os;
pub mod surface;
pub mod targets;

pub use orchestrator::{Outcome, run};
pub use surface::{HelperCommand, MenuSurface, Target};
