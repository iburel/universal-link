// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The official keyboard and mouse engine: the exclusive `input-backend`
//! component behind the Core's routed `input.*` facade. Its design is
//! doc/input-sharing.md, the letter this crate follows rule by rule, on top of
//! the generic Core primitives (`peers.send` for a gesture, `peers.channel` for
//! the live flow, the facade for the interface).
//!
//! Supervised-component contract: see `daemon/src/supervisor.rs` and
//! `src/main.rs` (which is wiring only). The lib holds everything testable,
//! which here means everything except the OS itself:
//!
//! - [`backend`]: the platform seam, and [`fake`] the double every test runs
//!   against. The real backends land behind it in their own ticket.
//! - [`wire`]: the dialect spoken over the live channel, frozen.
//! - [`plane`]: the signed, replicated layout document and its deterministic
//!   merge. [`graph`]: the crossing graph derived from it, and the guards.
//! - [`keys`]: key resolution, the injection sequence, and modifier hygiene.
//! - [`session`]: the session state machine, the exclusion, the coalescing, and
//!   the teardown every one of the channel's ten deaths goes through.
//! - [`orchestrator`]: the event loop behind the facade.
//! - [`store`] and [`identity`]: the engine's own persistence. The Core stores
//!   nothing about input.

pub mod backend;
pub mod fake;
pub mod graph;
pub mod identity;
pub mod keys;
pub mod orchestrator;
pub mod os;
pub mod plane;
pub mod session;
pub mod settings;
pub mod store;
pub mod wire;

// The platform halves, one per OS, selected at compile time by [`os::create`]
// exactly as `clipboard/src/os.rs` selects its own. Each one is the OS event loop
// for its platform plus a `Clone` handle over the seam; none of them is reachable
// from another platform's build.
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "linux")]
pub mod wayland;
#[cfg(target_os = "linux")]
pub mod wayland_portal;
#[cfg(windows)]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod x11;

pub use backend::{Capabilities, InputBackend, Monitor, Problem};
pub use orchestrator::{Outcome, SERVED_METHODS, run, supervised};
pub use store::Store;
