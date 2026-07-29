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

/// Topics the GUI asks for and can do without (`ClientConfig::optional_topics`).
///
/// `pairing` came after the first releases, and the Core subscribes all or
/// nothing: asking a Core that predates it would fail the whole subscription and
/// the interface would never connect — a hard stop, on a machine where the old
/// Core is simply still running from the previous session. It is asked for here
/// instead, and the pairing screens report themselves unavailable if the Core
/// turns out not to know the methods either (`-32601`). To be promoted to
/// [`GUI_TOPICS`] once no such Core is in the field.
pub const GUI_OPTIONAL_TOPICS: [&str; 1] = ["pairing"];

#[cfg(test)]
mod tests {
    /// The interface displays a version, and it can only take it from
    /// `ui/package.json` (substituted into the bundle by `vite.config.ts`): a
    /// webview has no way to read this crate's. So the two have to be bumped
    /// together — and a release bumps fifteen files by hand. This is what catches
    /// the one that was forgotten, before the app claims a version it is not.
    #[test]
    fn the_interface_and_this_binary_report_the_same_version() {
        let package_json = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/package.json");
        let ui: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&package_json).expect("ui/package.json is readable"),
        )
        .expect("ui/package.json is JSON");

        assert_eq!(
            ui["version"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "gui/ui/package.json and gui/Cargo.toml disagree: the app would \
             display one version and be another"
        );
    }
}
