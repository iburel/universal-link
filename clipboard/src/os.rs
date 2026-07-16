// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Per-OS backend construction. The real backends land one brick per platform —
//! X11/XWayland (brick 2), Windows (brick 3), macOS (brick 4) — each binding the
//! OS clipboard loop to the main thread and exposing a [`crate::ClipboardBackend`]
//! handle plus a `BackendEvent` stream. Until a platform's backend exists there
//! is nothing to drive on it: [`create`] reports [`Unsupported`], `main` exits
//! cleanly, and the supervisor does not yet register the component.
//!
//! The orchestrator ([`crate::run`]) is complete and frozen by the integration
//! suite against a real Core in the meantime.

/// No OS clipboard backend is available for this platform yet.
#[derive(Debug)]
pub struct Unsupported;

/// Builds the platform clipboard backend. Brick 1 has no backend on any
/// platform, so this always reports [`Unsupported`]; the `Ok` type is
/// uninhabited on purpose, so callers only need to handle the error until a
/// backend lands.
pub fn create() -> Result<std::convert::Infallible, Unsupported> {
    Err(Unsupported)
}
