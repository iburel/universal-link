// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Tauri shell of the GUI: a thin bridge between the webview and the Core via
//! `universallink-ipc-client`. Contract pinned by the `tests/api/` suite.
//!
//! The shell has NO business logic. `core_request` proxies the JSON-RPC in
//! full — the Core is the sole authority (validation, scopes): a method added
//! to the Core is available without touching anything here. The client's
//! events are relayed to the webview ("core:connection", "core:notification")
//! and `connection_status` exposes the fail-closed snapshot. The frontend
//! (`ui/`) holds the display state; the binary (`main.rs`) holds the
//! production config.

mod bridge;
mod supervise;

pub use bridge::shell;
pub use supervise::{bundled_core_path, register_autostart, spawn_core, stabilize_core_path};

/// Scopes requested by the official GUI (production binary).
/// `files.send`: the user sends files by dropping them onto a device;
/// `transfers.read`: track the progress of those sends (topic `transfers`).
/// The GUI only DISPLAYS outgoing transfers, but the topic has no direction
/// filter — incoming notifications are ignored.
pub const GUI_SCOPES: [&str; 7] = [
    "session.read",
    "session.manage",
    "devices.read",
    "devices.manage",
    "files.send",
    "transfers.read",
    "components.approve",
];

/// Topics subscribed to by the official GUI. The `component.pending`
/// notifications have no topic: they follow the `gui` role.
pub const GUI_TOPICS: [&str; 3] = ["session", "devices", "transfers"];
