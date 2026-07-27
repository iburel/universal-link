// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Integration suite of the contextual-menu manager, against a real Core and a
//! real server. One binary so the modules share the harness (see `support`).

mod clicks;
/// The real desktop artifacts, and the command lines the file managers run.
#[cfg(target_os = "linux")]
mod linux;
mod manager;
mod support;
/// Same, for the registry cascade and the "Send to" shortcuts.
#[cfg(target_os = "windows")]
mod windows;
