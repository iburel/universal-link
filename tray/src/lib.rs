// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The system-tray component: the async brain and its testable pieces.
//!
//! A supervised component must (see `daemon/src/supervisor.rs`, "Contract of a
//! supervised component"): find the Core at `UNIVERSALLINK_IPC_PATH`, read its
//! spawn token from the first line of standard input, keep that standard input
//! open (its EOF means "stop"), and **exit if it loses its IPC connection** —
//! the spawn token is single-use, so a reconnection would fail; exiting lets
//! the supervisor restart us with a fresh token.
//!
//! The platform tray (event loop, icon, menu) lives in `main`; this module
//! holds the async brain and the pure helpers it uses, so the exit conditions
//! (the contract) and the status mapping are unit-tested without a real Core.

use std::future::Future;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use universallink_ipc_client::{Client, Event};

/// Why the brain's loop ended — mapped by `main` to a process exit code.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Standard input closed: the supervisor asked us to stop. The only
    /// graceful-stop channel that exists on all three OSes. Exit success.
    StdinClosed,
    /// The IPC connection dropped after having been established. The spawn
    /// token is single-use — we exit and the supervisor restarts us with a
    /// fresh one.
    ConnectionLost,
    /// The Core announced an incompatible API version: retrying will not heal
    /// it. Exit.
    Incompatible,
    /// The client task ended on its own (no `Client` left).
    ClientEnded,
}

/// A command from the tray UI (a menu click) to the async brain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCommand {
    /// "Open UniversalLink" — bring up the GUI (wired in a later block).
    Open,
    /// "Quit" — stop the whole Core (its teardown then closes our stdin).
    Quit,
}

/// What the icon reflects. Minimal profile: one icon, a tooltip string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayStatus {
    Connecting,
    NotConfigured,
    SignedOut,
    Offline,
    Online,
}

impl TrayStatus {
    /// Tooltip shown on hover.
    pub fn tooltip(self) -> &'static str {
        match self {
            TrayStatus::Connecting => "UniversalLink — connecting…",
            TrayStatus::NotConfigured => "UniversalLink — not set up",
            TrayStatus::SignedOut => "UniversalLink — signed out",
            TrayStatus::Offline => "UniversalLink — offline",
            TrayStatus::Online => "UniversalLink — connected",
        }
    }

    /// Derives the status from a `session.status` result or a `session.changed`
    /// payload. `session.changed` omits `configured` — a live session implies a
    /// configured Core, so its absence means "assume configured".
    fn from_session(v: &Value) -> TrayStatus {
        let configured = v["configured"].as_bool().unwrap_or(true);
        let logged_in = v["logged_in"].as_bool().unwrap_or(false);
        let server_connected = v["server_connected"].as_bool().unwrap_or(false);
        // Connection first: a live session means "connected" even when
        // `configured` is false. A session carries its own server URL and
        // reconnects without a config.json (only a NEW login needs one), so
        // `configured` only distinguishes "never set up" from "signed out" when
        // there is no session at all.
        if server_connected {
            TrayStatus::Online
        } else if logged_in {
            TrayStatus::Offline
        } else if configured {
            TrayStatus::SignedOut
        } else {
            TrayStatus::NotConfigured
        }
    }
}

/// One step of the loop, derived from an IPC event. Pure, so the exit
/// conditions — the supervised-component contract — are unit-tested.
enum Step {
    /// Connection established: fetch the initial `session.status`.
    Connected,
    /// A `session.changed` payload to reflect.
    Status(Value),
    /// A connected-but-uninteresting notification: nothing to do.
    Idle,
    /// The loop must end.
    Exit(Outcome),
}

fn classify(event: Option<Event>) -> Step {
    match event {
        Some(Event::Connected { .. }) => Step::Connected,
        Some(Event::Notification { method, params }) if method == "session.changed" => {
            Step::Status(params)
        }
        Some(Event::Notification { .. }) => Step::Idle,
        Some(Event::Disconnected) => Step::Exit(Outcome::ConnectionLost),
        Some(Event::Incompatible { .. }) => Step::Exit(Outcome::Incompatible),
        None => Step::Exit(Outcome::ClientEnded),
    }
}

/// The async brain: consumes the Core's `events`, the standard-input EOF signal
/// and the UI `commands`; reports status through `on_status`. Returns why it
/// ended.
///
/// UI-agnostic on purpose (`on_status` is a plain closure, no windowing type),
/// so `main` bridges it to the tao event loop while the tests keep the pure
/// pieces (`classify`, `TrayStatus::from_session`) verifiable without a Core.
pub async fn run(
    client: Client,
    mut events: mpsc::Receiver<Event>,
    stdin_closed: impl Future<Output = ()>,
    mut commands: mpsc::Receiver<UiCommand>,
    on_status: impl Fn(TrayStatus),
) -> Outcome {
    tokio::pin!(stdin_closed);
    on_status(TrayStatus::Connecting);
    loop {
        tokio::select! {
            _ = &mut stdin_closed => return Outcome::StdinClosed,
            command = commands.recv() => match command {
                // The UI is gone (its sender dropped): nothing left to serve.
                None => return Outcome::ClientEnded,
                Some(UiCommand::Quit) => {
                    // Ask the Core to stop the whole service. Its orderly
                    // teardown closes our standard input, and the StdinClosed
                    // branch exits us — the supervisor stops us gracefully
                    // rather than seeing a self-exit it would restart. Offline,
                    // this is a no-op (there is nothing to talk to).
                    let _ = client.request("system.shutdown", json!({})).await;
                }
                Some(UiCommand::Open) => open_gui(),
            },
            event = events.recv() => match classify(event) {
                Step::Connected => {
                    // session.changed only fires on a CHANGE, so the current
                    // state is fetched once on connection.
                    if let Ok(status) = client.request("session.status", json!({})).await {
                        on_status(TrayStatus::from_session(&status));
                    }
                }
                Step::Status(payload) => on_status(TrayStatus::from_session(&payload)),
                Step::Idle => {}
                Step::Exit(outcome) => return outcome,
            },
        }
    }
}

/// Launches the GUI from the target it recorded at startup (the tray runs from
/// the Core's durable copy and cannot otherwise find it). Best-effort and
/// fire-and-forget; a missing or stale record just means nothing opens.
fn open_gui() {
    let Some(endpoint) = universallink_paths::production_endpoint() else {
        eprintln!("[universallink-tray] cannot resolve the config directory");
        return;
    };
    let record = endpoint.gui_launch_path();
    let target = match std::fs::read_to_string(&record) {
        Ok(target) if !target.trim().is_empty() => target.trim().to_string(),
        _ => {
            eprintln!(
                "[universallink-tray] no recorded GUI launch path ({})",
                record.display()
            );
            return;
        }
    };
    // macOS: `open` activates an existing instance rather than duplicating it.
    // Elsewhere: run the recorded target directly. Detached from our standard
    // input (the supervisor's token pipe) so the GUI does not inherit it.
    let mut command = if cfg!(target_os = "macos") {
        let mut open = std::process::Command::new("open");
        open.arg(&target);
        open
    } else {
        std::process::Command::new(&target)
    };
    command.stdin(std::process::Stdio::null());
    if let Err(e) = command.spawn() {
        eprintln!("[universallink-tray] cannot launch the GUI ({target}): {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_events_to_steps() {
        assert!(matches!(
            classify(Some(Event::Connected {
                granted_scopes: vec![],
                api_version: 1
            })),
            Step::Connected
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "session.changed".into(),
                params: json!({ "logged_in": true }),
            })),
            Step::Status(_)
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "device.online".into(),
                params: Value::Null,
            })),
            Step::Idle
        ));
        // The exit conditions of the supervised-component contract.
        assert!(matches!(
            classify(Some(Event::Disconnected)),
            Step::Exit(Outcome::ConnectionLost)
        ));
        assert!(matches!(
            classify(Some(Event::Incompatible { api_version: 2 })),
            Step::Exit(Outcome::Incompatible)
        ));
        assert!(matches!(classify(None), Step::Exit(Outcome::ClientEnded)));
    }

    #[test]
    fn status_reflects_the_session_fields() {
        let status = |v| TrayStatus::from_session(&v);
        assert_eq!(
            status(json!({ "configured": false, "logged_in": false, "server_connected": false })),
            TrayStatus::NotConfigured
        );
        assert_eq!(
            status(json!({ "configured": true, "logged_in": false, "server_connected": false })),
            TrayStatus::SignedOut
        );
        assert_eq!(
            status(json!({ "configured": true, "logged_in": true, "server_connected": false })),
            TrayStatus::Offline
        );
        assert_eq!(
            status(json!({ "configured": true, "logged_in": true, "server_connected": true })),
            TrayStatus::Online
        );
        // session.changed omits `configured`: a live session implies configured.
        assert_eq!(
            status(json!({ "logged_in": true, "server_connected": true })),
            TrayStatus::Online
        );
        // A live session with no config.json (a new login isn't possible, but
        // the session reconnects on its own URL): still "connected", never
        // "not set up".
        assert_eq!(
            status(json!({ "configured": false, "logged_in": true, "server_connected": true })),
            TrayStatus::Online
        );
        assert_eq!(
            status(json!({ "configured": false, "logged_in": true, "server_connected": false })),
            TrayStatus::Offline
        );
    }

    #[test]
    fn every_status_has_a_tooltip() {
        for status in [
            TrayStatus::Connecting,
            TrayStatus::NotConfigured,
            TrayStatus::SignedOut,
            TrayStatus::Offline,
            TrayStatus::Online,
        ] {
            assert!(status.tooltip().contains("UniversalLink"));
        }
    }
}
