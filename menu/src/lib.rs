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
//! — reachable (the Core's one presence flag: server presence, or the machine
//! heard on the local network — which is why the menu survives a dead internet),
//! attested, not us, not the phone. No manager means no entry either: the
//! surfaces are emptied at startup and at graceful shutdown.
//!
//! The accepted residual (decision, 2026-07-27): a manager that *crashes* leaves
//! its artifacts behind until the supervisor restarts it. A click on one then
//! finds no channel and fails silently, which is why the helper needs no error
//! dialog.
//!
//! # Bricks
//!
//! Brick 1 was the manager, the channel and the seam; brick 2 the Linux surfaces
//! ([`os::linux`]: KDE ServiceMenus for Dolphin, Nautilus scripts); brick 3 the
//! Windows ones ([`os::windows`]: the classic shortcut menu's cascade, for files and
//! for folders, plus one "Send to" shortcut per device); brick 4 the macOS one
//! ([`os::macos`]: one Automator workflow per device in `~/Library/Services`, shown
//! in Finder's Services submenu). All are frozen by `tests/api/` against a real Core
//! — down to running the courier from the command line the artifacts actually carry.
//! Every OS now has a surface, so [`os::create`] reports [`os::Unsupported`] only on
//! a platform this project does not ship to.
//!
//! Brick 5 is the last, and it is one rule: **the marker is the authority, the
//! container is the scope**. Every surface drops what it wrote by enumerating the
//! container its reader reads — a directory, a `shell` key — and deleting whatever
//! carries our marker, rather than unlinking the names this version happens to
//! write. So renaming an artifact heals itself on the next startup, and no list of
//! retired names has to be kept in step with the code: the four packaging lists
//! below are the standing demonstration of what becomes of a list nothing
//! cross-checks. This is worth a rule because none of those containers is ours
//! alone, and a stale artifact in one is not dormant — it is a live menu entry,
//! frozen on the device list of the day it was written, whose clicks the manager
//! refuses one by one. KIO reads every `.desktop` in its directory and deduplicates
//! them by FILE NAME, so a rename is precisely what leaves a second entry behind;
//! Explorer reads every subkey of `shell`; Nautilus reads `scripts/` itself as a
//! menu as well as our submenu inside it. The boundary is directories: one carries
//! no marker, so ours is removed only when a sweep leaves it empty, and someone
//! else's is never inferred to be ours from what happens to be inside it.
//!
//! Brick 4 is where the escaping obligation got its fourth answer, and the first that
//! is TWO answers at once: the command line is a shell script *inside* a plist
//! string, so brick 2's single-quoting runs first and XML escaping wraps it. It also
//! contributed the only surface whose label the system itself resolves by — two
//! devices with one name are not merely confusing there, they are ambiguous to macOS
//! — and the only one that TELLS the OS it changed (`NSUpdateDynamicServices`), which
//! buys immediacy rather than correctness: macOS follows the directory by itself in
//! about seven seconds, and those are seconds in which a menu offers an entry whose
//! click does nothing.
//!
//! Brick 3 is also where a click stopped being one process. The Windows classic menu
//! invokes a verb ONCE PER SELECTED FILE, so one gesture arrives as a burst of
//! couriers; `clicks` batches them into a single `files.send` (and the batching runs
//! on every platform, so every CI job exercises it). The other two things that brick
//! taught the component:
//! - **the escaping obligation has a third answer.** A registry value is a counted
//!   string, so nothing can break out of one — but the shell reads `&` in a label as
//!   a mnemonic and a leading `@` as a resource reference, and it substitutes field
//!   codes (`%1`, `%V`) in a command line before any program sees it. And on the same
//!   platform, the "Send to" label is a FILE NAME again, which is the Nautilus
//!   answer with Windows' rules. See [`os::windows`].
//! - **the courier is a GUI-subsystem binary** (`main.rs`): a console-subsystem
//!   process started by Explorer flashes a console window at every click, and one
//!   started that way may have no standard handles at all — where `println!` panics.
//!
//! Brick 2 is where the component stopped being inert, so the four packaging
//! obligations below are DONE for all three platforms — a surface that skips one
//! ships as something nothing launches:
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
//! 4. any build-time system library, in BOTH `ci.yml` and `release.yml`. None of the
//!    three needed one: Linux writes plain files under `$XDG_DATA_HOME`, Windows uses
//!    the registry and COM, and macOS links AppKit — a framework every macOS has, and
//!    nothing a runner has to install.
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
mod clicks;
mod orchestrator;
pub mod os;
pub mod surface;
pub mod targets;

pub use orchestrator::{Outcome, run};
pub use surface::{HelperCommand, MenuSurface, Target};
