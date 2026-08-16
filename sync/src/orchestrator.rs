// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The engine's event loop: consumes the IPC client's events, answers the
//! facade's forwarded `sync.*` calls from the store, and ends on the
//! supervised-component contract's terminal conditions.

use std::future::Future;

use onedevice_ipc_client::{Client, Event, RequestError, RequestId};
use tokio::sync::mpsc;

use crate::store::Store;

/// The facade methods the engine serves today. The list grows brick by brick
/// toward the frozen vocabulary (doc/sync-engine.md, section 10); a `sync.*`
/// name absent from it is refused with `-32601` by the IPC client itself, and
/// the Core relays that refusal verbatim to the caller.
pub const SERVED_METHODS: [&str; 1] = ["sync.status"];

/// Why the loop ended - mapped by `main` to a process exit code.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Standard input closed: the supervisor asked us to stop. Exit success.
    StdinClosed,
    /// The IPC connection dropped after being established. The spawn token is
    /// single-use - exit so the supervisor restarts us with a fresh one.
    ConnectionLost,
    /// The Core announced an incompatible API version: retrying will not heal
    /// it.
    Incompatible,
    /// The client task ended on its own (no `Client` left).
    ClientEnded,
}

/// One step derived from an IPC event. Pure, so the exit conditions - the
/// supervised-component contract - are unit-tested without a Core. `Status`
/// carries a [`RequestId`], which only the client crate can mint; the unit
/// tests therefore cover every variant but that one, which the integration
/// suite drives.
enum Action {
    /// A forwarded `sync.status`: answer the snapshot from the store.
    Status(RequestId),
    /// A request in `served_methods` that this dispatch does not handle:
    /// impossible while the two lists agree, refused honestly if they ever
    /// drift (a dropped reply would burn the caller's whole facade budget).
    Unsupported(RequestId),
    /// A connected-but-uninteresting event: nothing to do yet. Connection
    /// establishment lands here too - the engine's snapshot lives in its own
    /// store, so there is nothing to resynchronize from the Core until the
    /// reconciliation bricks arrive.
    Idle,
    /// The loop must end.
    Exit(Outcome),
}

fn classify(event: Option<Event>) -> Action {
    match event {
        Some(Event::Request { id, method, .. }) if method == "sync.status" => Action::Status(id),
        // The client only delivers requests whose method is in
        // [`SERVED_METHODS`]: reaching this arm means the two lists drifted.
        Some(Event::Request { id, .. }) => Action::Unsupported(id),
        Some(Event::Connected { .. }) | Some(Event::Notification { .. }) => Action::Idle,
        Some(Event::Disconnected) => Action::Exit(Outcome::ConnectionLost),
        Some(Event::Incompatible { .. }) => Action::Exit(Outcome::Incompatible),
        None => Action::Exit(Outcome::ClientEnded),
    }
}

/// Runs the engine until a terminal condition. Consumes the Core `events`
/// stream; `stdin_closed` resolves on the supervisor's graceful-stop signal.
pub async fn run(
    client: Client,
    mut events: mpsc::Receiver<Event>,
    store: Store,
    stdin_closed: impl Future<Output = ()>,
) -> Outcome {
    tokio::pin!(stdin_closed);

    loop {
        tokio::select! {
            biased;
            () = &mut stdin_closed => break Outcome::StdinClosed,
            event = events.recv() => match classify(event) {
                // Replies ride their own task: `Client::respond` awaits the
                // actual socket write, and a Core that stopped draining must
                // not hold the stop signal past the supervisor's grace.
                Action::Status(id) => {
                    let client = client.clone();
                    let snapshot = store.status();
                    tokio::spawn(async move { respond(client, id, snapshot).await });
                }
                Action::Unsupported(id) => {
                    let client = client.clone();
                    tokio::spawn(async move {
                        swallow_stale(client.respond_error(id, "SYNC_UNSUPPORTED").await);
                    });
                }
                Action::Idle => {}
                Action::Exit(outcome) => break outcome,
            },
        }
    }
}

/// Answers `sync.status` with the snapshot, assembled from local state
/// alone before the task was spawned.
async fn respond(client: Client, id: RequestId, snapshot: serde_json::Value) {
    swallow_stale(client.respond(id, snapshot).await);
}

/// A stale request id (its connection dropped mid-flight) is expected
/// around a reconnect; anything else deserves a trace.
fn swallow_stale(result: Result<(), RequestError>) {
    if let Err(e) = result
        && !matches!(e, RequestError::Disconnected)
    {
        eprintln!("[1device-sync] reply failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_conditions_map_to_their_outcomes() {
        assert!(matches!(
            classify(Some(Event::Disconnected)),
            Action::Exit(Outcome::ConnectionLost)
        ));
        assert!(matches!(
            classify(Some(Event::Incompatible { api_version: 2 })),
            Action::Exit(Outcome::Incompatible)
        ));
        assert!(matches!(classify(None), Action::Exit(Outcome::ClientEnded)));
    }

    #[test]
    fn connection_and_notifications_are_idle_for_now() {
        assert!(matches!(
            classify(Some(Event::Connected {
                granted_scopes: Vec::new(),
                api_version: 1,
            })),
            Action::Idle
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "devices.updated".into(),
                params: serde_json::json!({}),
            })),
            Action::Idle
        ));
    }
}
