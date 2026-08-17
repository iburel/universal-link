// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The system-tray component: the async brain and its testable pieces.
//!
//! A supervised component must (see `daemon/src/supervisor.rs`, "Contract of a
//! supervised component"): find the Core at `ONEDEVICE_IPC_PATH`, read its
//! spawn token from the first line of standard input, keep that standard input
//! open (its EOF means "stop"), and **exit if it loses its IPC connection** —
//! the spawn token is single-use, so a reconnection would fail; exiting lets
//! the supervisor restart us with a fresh token.
//!
//! The platform tray (event loop, icon, menu) lives in `main`; this module
//! holds the async brain and the pure helpers it uses, so the exit conditions
//! (the contract) and the status mapping are unit-tested without a real Core.

use std::future::Future;

use onedevice_ipc_client::{Client, Event};
use serde_json::{Value, json};
use tokio::sync::mpsc;

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
    /// "Open 1Device" — bring up the GUI (wired in a later block).
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

/// Whether this computer's keyboard and mouse are somewhere else, or somebody
/// else's are here. The epic's rule: a session is visible in the tray for the
/// whole time it lasts, on BOTH sides, so nobody is ever driving a machine or
/// being driven without a standing sign of it outside the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputSession {
    /// No session: this computer's keyboard is its own.
    None,
    /// This computer's keyboard, and its mouse unless `keys`, are on another one.
    Driving { keys: bool },
    /// Another computer is using this one's keyboard, and its mouse unless `keys`.
    Driven { keys: bool },
}

impl InputSession {
    /// The line, or `None` when there is no session to report. `who` is the other
    /// computer's name when the snapshot carried one.
    ///
    /// The MODE is in here and not a detail: a keyboard-only session (the offer
    /// the Input tab makes on every slow path) never moved the mouse, and a tray
    /// saying it did would be inventing half a session.
    pub fn line(&self, who: Option<&str>) -> Option<String> {
        let there = who.unwrap_or("another computer");
        match self {
            InputSession::None => None,
            InputSession::Driving { keys } => Some(if *keys {
                format!("your keyboard is on {there}")
            } else {
                format!("your keyboard and mouse are on {there}")
            }),
            InputSession::Driven { keys } => Some(if *keys {
                format!("{there} is using your keyboard")
            } else {
                format!("{there} is using your keyboard and mouse")
            }),
        }
    }

    /// Reads the live session off an `input.status` result or an `input.updated`
    /// payload's `state`: the same object either way, which is the point of the
    /// engine publishing its whole snapshot. Anything unexpected is no session,
    /// fail-closed in the direction of saying less rather than of claiming a
    /// session that is not there.
    ///
    /// The name comes out of the same payload: the snapshot carries every device's
    /// name (`devices[].name`), so naming the computer costs the tray no scope it
    /// does not already hold. An earlier version of this file claimed the opposite
    /// and it was simply wrong.
    fn from_input(state: &Value) -> (InputSession, Option<String>) {
        let Some(session) = state.get("session").filter(|s| s.is_object()) else {
            return (InputSession::None, None);
        };
        let keys = session.get("mode").and_then(Value::as_str) == Some("keys");
        let live = match session.get("direction").and_then(Value::as_str) {
            Some("out") => InputSession::Driving { keys },
            Some("in") => InputSession::Driven { keys },
            _ => return (InputSession::None, None),
        };
        let name = session
            .get("device_id")
            .and_then(Value::as_str)
            .and_then(|id| {
                state
                    .get("devices")
                    .and_then(Value::as_array)?
                    .iter()
                    .find(|d| d.get("device_id").and_then(Value::as_str) == Some(id))?
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string)
            });
        (live, name)
    }
}

/// Everything the tray shows at one moment: the Core's connection state and
/// whether a keyboard is away. One value rather than two callbacks, so the two
/// halves can never be drawn from different moments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    pub status: TrayStatus,
    pub input: InputSession,
    /// The other computer of the live session, when the snapshot named it.
    pub peer: Option<String>,
}

impl TrayView {
    /// What is shown before anything has been read.
    pub fn connecting() -> TrayView {
        TrayView {
            status: TrayStatus::Connecting,
            input: InputSession::None,
            peer: None,
        }
    }

    /// Tooltip shown on hover. A live session takes it over: it is the one thing
    /// here that is happening rather than merely true, and the connection state
    /// is a click away in the menu.
    pub fn tooltip(&self) -> String {
        match self.input.line(self.peer.as_deref()) {
            Some(line) => format!("1Device: {line}"),
            None => self.status.tooltip().to_string(),
        }
    }

    /// The menu's first line, which is disabled: it is a state, not a command.
    ///
    /// It exists because on Linux the tray's tooltip is a silent no-op (the
    /// libappindicator backend of `tray-icon` discards it), so a tooltip alone
    /// would leave a whole platform with no sign that its keyboard is away. A
    /// menu label is text every platform draws.
    pub fn menu_line(&self) -> String {
        match self.input.line(self.peer.as_deref()) {
            Some(mut said) => {
                if let Some(first) = said.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                said
            }
            None => self.status.tooltip().to_string(),
        }
    }
}

impl TrayStatus {
    /// Tooltip shown on hover.
    pub fn tooltip(self) -> &'static str {
        match self {
            TrayStatus::Connecting => "1Device: connecting…",
            TrayStatus::NotConfigured => "1Device: not set up",
            TrayStatus::SignedOut => "1Device: signed out",
            TrayStatus::Offline => "1Device: offline",
            TrayStatus::Online => "1Device: connected",
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
    /// Connection established: fetch the initial `session.status`, and the
    /// engine's snapshot if this Core granted the scope for it.
    Connected { granted: Vec<String> },
    /// A `session.changed` payload to reflect.
    Status(Value),
    /// An `input.updated` payload: the engine's whole state, from which only the
    /// live session's direction is read here.
    Input(Value),
    /// A connected-but-uninteresting notification: nothing to do.
    Idle,
    /// The loop must end.
    Exit(Outcome),
}

fn classify(event: Option<Event>) -> Step {
    match event {
        Some(Event::Connected { granted_scopes, .. }) => Step::Connected {
            granted: granted_scopes,
        },
        Some(Event::Notification { method, params }) if method == "session.changed" => {
            Step::Status(params)
        }
        // The engine's own topic. `input.refused` is deliberately not here: a
        // refusal is a sentence for the interface to say, and a tray cannot say
        // a sentence without stealing the focus for it.
        Some(Event::Notification { method, params }) if method == "input.updated" => {
            Step::Input(params.get("state").cloned().unwrap_or(Value::Null))
        }
        Some(Event::Notification { .. }) => Step::Idle,
        // The tray serves no Core→component method (empty served_methods).
        Some(Event::Request { .. }) => Step::Idle,
        Some(Event::Disconnected) => Step::Exit(Outcome::ConnectionLost),
        Some(Event::Incompatible { .. }) => Step::Exit(Outcome::Incompatible),
        None => Step::Exit(Outcome::ClientEnded),
    }
}

/// The async brain: consumes the Core's `events`, the standard-input EOF signal
/// and the UI `commands`; reports what to show through `on_view`. Returns why it
/// ended.
///
/// UI-agnostic on purpose (`on_view` is a plain closure, no windowing type), so
/// `main` bridges it to the tao event loop while the tests keep the pure pieces
/// (`classify`, `TrayStatus::from_session`, `InputSession::from_input`)
/// verifiable without a Core.
pub async fn run(
    client: Client,
    mut events: mpsc::Receiver<Event>,
    stdin_closed: impl Future<Output = ()>,
    mut commands: mpsc::Receiver<UiCommand>,
    on_view: impl Fn(TrayView),
) -> Outcome {
    tokio::pin!(stdin_closed);
    // The two halves are remembered here, because each is published on its own
    // and the tray draws both at once.
    let mut view = TrayView::connecting();
    let mut last: Option<TrayView> = None;
    say(&on_view, &view, &mut last);
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
                Step::Connected { granted } => {
                    // session.changed only fires on a CHANGE, so the current
                    // state is fetched once on connection.
                    if let Ok(status) = client.request("session.status", json!({})).await {
                        view.status = TrayStatus::from_session(&status);
                    }
                    // Same rule for the engine, and the same reason: a window
                    // (or a tray) that starts in the middle of a session has to
                    // read the state rather than wait for it to change. Only if
                    // this Core granted the scope, which one older than the
                    // facade does not; and a failure is ORDINARY here, since
                    // `COMPONENT_ABSENT` is what a machine with no engine
                    // answers, and it means exactly "no session".
                    // Published BEFORE the engine is asked, deliberately: that
                    // second request is a facade forward with a ten second budget
                    // behind it, and an engine still starting up answers it late.
                    // The tray would otherwise sit on "connecting…" with the
                    // connection state already in its hand.
                    say(&on_view, &view, &mut last);
                    // Same rule for the engine, and the same reason: a window
                    // (or a tray) that starts in the middle of a session has to
                    // read the state rather than wait for it to change. Only if
                    // this Core granted the scope, which one older than the
                    // facade does not; and a failure is ORDINARY here, since
                    // `COMPONENT_ABSENT` is what a machine with no engine
                    // answers, and it means exactly "no session".
                    if granted.iter().any(|s| s == "input.read")
                        && let Ok(state) = client.request("input.status", json!({})).await
                    {
                        (view.input, view.peer) = InputSession::from_input(&state);
                    }
                }
                Step::Status(payload) => view.status = TrayStatus::from_session(&payload),
                Step::Input(state) => {
                    (view.input, view.peer) = InputSession::from_input(&state);
                }
                Step::Idle => {}
                Step::Exit(outcome) => return outcome,
            },
        }
        say(&on_view, &view, &mut last);
    }
}

/// Publishes only what CHANGED. `input.updated` is not "something the tray shows
/// moved": the engine republishes its whole snapshot for every fresh round trip
/// measurement, which is once a second during a session and once every three
/// seconds per warm peer for ever. Without this the tray would rewrite its menu
/// item and its tooltip on that heartbeat, and on Linux that is a menu update over
/// DBus, possibly under the cursor of somebody reading it.
fn say(on_view: &impl Fn(TrayView), view: &TrayView, last: &mut Option<TrayView>) {
    if last.as_ref() == Some(view) {
        return;
    }
    *last = Some(view.clone());
    on_view(view.clone());
}

/// Launches the GUI from the target it recorded at startup (the tray runs from
/// the Core's durable copy and cannot otherwise find it). Best-effort and
/// fire-and-forget; a missing or stale record just means nothing opens.
fn open_gui() {
    let Some(endpoint) = onedevice_paths::production_endpoint() else {
        eprintln!("[1device-tray] cannot resolve the config directory");
        return;
    };
    let record = endpoint.gui_launch_path();
    let target = match std::fs::read_to_string(&record) {
        Ok(target) if !target.trim().is_empty() => target.trim().to_string(),
        _ => {
            eprintln!(
                "[1device-tray] no recorded GUI launch path ({})",
                record.display()
            );
            return;
        }
    };
    // macOS: `open` activates an existing instance rather than duplicating it —
    // which is what we want, and which only holds because this process is not
    // itself registered as that application. It ships in a bundle of its own
    // (`Contents/Frameworks/1DeviceTray.app`, see
    // `daemon::supervisor::helper_bundle_program`); a tray running from
    // `Contents/MacOS` *is* `org.onedevice.gui` to Launch Services, and this
    // `open` would then activate us and raise no window at all.
    //
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
        eprintln!("[1device-tray] cannot launch the GUI ({target}): {e}");
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
            Step::Connected { .. }
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
            assert!(status.tooltip().contains("1Device"));
        }
    }

    /// The engine publishes its whole state; the direction, the mode and the other
    /// computer's name are all read out of it, and both sides of a session have to
    /// show it.
    #[test]
    fn the_session_is_read_off_the_engines_state() {
        let live = |v| InputSession::from_input(&v);
        let snapshot = |direction, mode| {
            json!({
                "session": { "device_id": "d_1", "direction": direction,
                             "mode": mode, "since": 1, "rtt_ms": 4 },
                "devices": [{ "device_id": "d_1", "name": "Desk" }],
            })
        };
        assert_eq!(
            live(snapshot("out", "full")),
            (
                InputSession::Driving { keys: false },
                Some("Desk".to_string())
            )
        );
        // The MODE, because a keyboard-only session never moved the mouse.
        assert_eq!(
            live(snapshot("out", "keys")),
            (
                InputSession::Driving { keys: true },
                Some("Desk".to_string())
            )
        );
        assert_eq!(
            live(snapshot("in", "full")),
            (
                InputSession::Driven { keys: false },
                Some("Desk".to_string())
            )
        );
        // A session whose device nobody names is still a session.
        assert_eq!(
            live(json!({ "session": { "direction": "out", "mode": "full" } })),
            (InputSession::Driving { keys: false }, None)
        );
        // Nothing to read: no session, rather than a session in some direction
        // this build does not know.
        for nothing in [
            json!({ "session": null }),
            json!({}),
            Value::Null,
            json!({ "session": { "direction": "sideways" } }),
        ] {
            assert_eq!(live(nothing), (InputSession::None, None));
        }
    }

    /// A live session owns the tooltip, and it says which way round it is, what
    /// really crossed, and which computer: the payload the tray already receives
    /// carries every device's name.
    #[test]
    fn a_keyboard_that_is_away_is_what_the_tray_says() {
        let view = |input, peer: Option<&str>| TrayView {
            status: TrayStatus::Online,
            input,
            peer: peer.map(str::to_string),
        };
        // No session: the tooltip is the connection state's own, whatever that
        // wording is.
        assert_eq!(
            view(InputSession::None, None).tooltip(),
            TrayStatus::Online.tooltip()
        );
        assert_eq!(
            view(InputSession::Driving { keys: false }, Some("Desk")).tooltip(),
            "1Device: your keyboard and mouse are on Desk"
        );
        assert_eq!(
            view(InputSession::Driving { keys: true }, Some("Desk")).tooltip(),
            "1Device: your keyboard is on Desk"
        );
        assert_eq!(
            view(InputSession::Driven { keys: false }, Some("Laptop")).tooltip(),
            "1Device: Laptop is using your keyboard and mouse"
        );
        assert_eq!(
            view(InputSession::Driven { keys: true }, Some("Laptop")).tooltip(),
            "1Device: Laptop is using your keyboard"
        );
        // Unnamed, and it still reads.
        assert_eq!(
            view(InputSession::Driving { keys: false }, None).tooltip(),
            "1Device: your keyboard and mouse are on another computer"
        );
        // The menu line carries the same fact, because on Linux the tooltip is
        // discarded by the tray backend and the menu is the only text drawn.
        assert_eq!(
            view(InputSession::Driving { keys: false }, Some("Desk")).menu_line(),
            "Your keyboard and mouse are on Desk"
        );
        assert_eq!(
            view(InputSession::Driven { keys: false }, Some("Laptop")).menu_line(),
            "Laptop is using your keyboard and mouse"
        );
        // And with no session it falls back to the connection state, so the line
        // is never blank.
        assert_eq!(
            view(InputSession::None, None).menu_line(),
            TrayStatus::Online.tooltip()
        );
        for status in [
            TrayStatus::Connecting,
            TrayStatus::NotConfigured,
            TrayStatus::SignedOut,
            TrayStatus::Offline,
            TrayStatus::Online,
        ] {
            for input in [
                InputSession::None,
                InputSession::Driving { keys: false },
                InputSession::Driven { keys: true },
            ] {
                let view = TrayView {
                    status,
                    input,
                    peer: None,
                };
                assert!(!view.menu_line().is_empty());
                assert!(!view.tooltip().is_empty());
            }
        }
    }

    /// The tray writes its menu item and its tooltip only when something it shows
    /// changed. The engine republishes its whole snapshot for every round trip
    /// measurement, which is a heartbeat rather than news.
    #[test]
    fn nothing_is_redrawn_for_a_state_that_did_not_change() {
        let drawn = std::sync::Mutex::new(Vec::new());
        let on_view = |view: TrayView| drawn.lock().expect("lock").push(view);
        let mut last = None;
        let view = TrayView::connecting();
        say(&on_view, &view, &mut last);
        say(&on_view, &view, &mut last);
        say(&on_view, &view, &mut last);
        assert_eq!(drawn.lock().expect("lock").len(), 1);

        let moved = TrayView {
            status: TrayStatus::Online,
            input: InputSession::Driving { keys: false },
            peer: Some("Desk".into()),
        };
        say(&on_view, &moved, &mut last);
        say(&on_view, &moved, &mut last);
        assert_eq!(drawn.lock().expect("lock").len(), 2);
    }

    /// The engine's topic is followed; its refusals are not the tray's to say.
    #[test]
    fn the_input_topic_is_classified_and_a_refusal_is_not() {
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "input.updated".into(),
                params: json!({ "state": { "session": null } }),
            })),
            Step::Input(_)
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "input.refused".into(),
                params: json!({ "device_id": "d_1", "code": "SECURE_INPUT", "count": 1 }),
            })),
            Step::Idle
        ));
        // The granted scopes ride the Connected step: the snapshot is only asked
        // for when this Core granted the right to read it.
        assert!(matches!(
            classify(Some(Event::Connected {
                granted_scopes: vec!["session.read".into(), "input.read".into()],
                api_version: 1,
            })),
            Step::Connected { granted } if granted.iter().any(|s| s == "input.read")
        ));
    }
}
