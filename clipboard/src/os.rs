// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Per-OS backend construction. The real backends land one brick per platform —
//! X11/XWayland (brick 2), Windows (brick 3), macOS (brick 4) — each binding the
//! OS clipboard loop to the main thread and exposing a [`crate::ClipboardBackend`]
//! handle plus a `BackendEvent` stream. On a platform without a backend yet
//! (and on Linux with no reachable X server), [`create`] reports [`Unsupported`],
//! `main` exits cleanly, and the supervisor does not register the component.
//!
//! The orchestrator ([`crate::run`]) is complete and frozen by the integration
//! suite against a real Core.

/// No OS clipboard backend is available (unsupported platform, or no X server).
#[derive(Debug)]
pub struct Unsupported;

/// The pieces a built backend hands back: the `Clone` handle the orchestrator
/// drives, the upcall stream it consumes, and the main-thread event loop `main`
/// pumps.
#[cfg(target_os = "linux")]
pub struct Created {
    pub handle: crate::x11::X11Backend,
    pub backend_events: tokio::sync::mpsc::Receiver<crate::backend::BackendEvent>,
    pub event_loop: crate::x11::X11Loop,
}

/// Builds the platform clipboard backend (Linux/X11). A connect failure — no X
/// server, i.e. not an X11 session — is reported as [`Unsupported`] so `main`
/// exits cleanly.
#[cfg(target_os = "linux")]
pub fn create() -> Result<Created, Unsupported> {
    crate::x11::create().map_err(|_| Unsupported)
}

/// No backend on this platform yet: the `Ok` type is uninhabited on purpose, so
/// callers only need to handle the error until a backend lands.
#[cfg(not(target_os = "linux"))]
pub fn create() -> Result<std::convert::Infallible, Unsupported> {
    Err(Unsupported)
}
