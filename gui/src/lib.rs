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
// `supervise` spawns an EXTERNAL Core, registers autostart and records launch
// targets — all desktop-only. The mobile shell (`gui-mobile`) embeds the Core
// in-process and never touches it; and it does not compile on Android (its
// inner helpers are gated per desktop OS). So the module — and the binary that
// uses it — is desktop-only.
#[cfg(not(target_os = "android"))]
mod supervise;

// The bridge is reused verbatim by the mobile shell (`gui-mobile`): same
// commands, same connection relay, over the embedded Core's socket.
pub use bridge::{
    CommandError, CoreState, ServerConfigForm, bridge_loop, connection_status, core_request,
    get_server_config, set_server_config, shell,
};
#[cfg(not(target_os = "android"))]
pub use supervise::{
    bundled_core_path, record_launch_target, register_autostart, spawn_core, stabilize_core_path,
};

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
