// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The system-tray component: skeleton and contract with the Core.
//!
//! A supervised component must (see `daemon/src/supervisor.rs`, "Contract of a
//! supervised component"): find the Core at `UNIVERSALLINK_IPC_PATH`, read its
//! spawn token from the first line of standard input, keep that standard input
//! open (its EOF means "stop"), and **exit if it loses its IPC connection** —
//! the spawn token is single-use, so a reconnection would fail; exiting lets
//! the supervisor restart us with a fresh token.
//!
//! Everything I/O lives in `main`; this module holds only the event loop and
//! its exit conditions, so the contract is tested deterministically against a
//! synthetic event stream — no real Core needed.

use std::future::Future;

use tokio::sync::mpsc;
use universallink_ipc_client::Event;

/// Why the tray's event loop ended — and, in `main`, how we exit.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Standard input closed: the supervisor asked us to stop. The only
    /// graceful-stop channel that exists on all three OSes. Exit success.
    StdinClosed,
    /// The IPC connection dropped after having been established. The spawn
    /// token is single-use — we cannot reconnect, so we exit and the supervisor
    /// restarts us with a fresh one.
    ConnectionLost,
    /// The Core announced an incompatible API version: retrying will not heal
    /// it. We exit; a version match is a deployment concern.
    Incompatible,
    /// The client task ended on its own (no `Client` left / permanent stop).
    ClientEnded,
}

/// Drives the tray until it must stop. Consumes the Core's `events` and the
/// `stdin_closed` signal (standard-input EOF).
///
/// Later building blocks react to `Connected` / `Notification` here (draw the
/// icon, reflect `session.status`); today the skeleton only needs the stop
/// conditions right.
pub async fn run(
    mut events: mpsc::Receiver<Event>,
    stdin_closed: impl Future<Output = ()>,
) -> Outcome {
    tokio::pin!(stdin_closed);
    loop {
        tokio::select! {
            _ = &mut stdin_closed => return Outcome::StdinClosed,
            event = events.recv() => match event {
                // The connection is up. Brick 3+: (re)draw the icon and read
                // session.status; the spawn token is now consumed, so any later
                // loss is terminal.
                Some(Event::Connected { .. }) => {}
                // Brick 4+: reflect session / device / transfer changes.
                Some(Event::Notification { .. }) => {}
                Some(Event::Disconnected) => return Outcome::ConnectionLost,
                Some(Event::Incompatible { .. }) => return Outcome::Incompatible,
                None => return Outcome::ClientEnded,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdin_eof_stops_gracefully() {
        // The events channel stays open (`_tx` held) so `recv` pends and the
        // only ready branch is the closed standard input.
        let (_tx, rx) = mpsc::channel::<Event>(8);
        let outcome = run(rx, std::future::ready(())).await;
        assert_eq!(outcome, Outcome::StdinClosed);
    }

    #[tokio::test]
    async fn a_lost_connection_makes_us_exit() {
        let (tx, rx) = mpsc::channel::<Event>(8);
        tx.send(Event::Connected {
            granted_scopes: vec!["session.read".into()],
            api_version: 1,
        })
        .await
        .unwrap();
        tx.send(Event::Disconnected).await.unwrap();
        // Standard input never closes: the loss is what stops us.
        let outcome = run(rx, std::future::pending::<()>()).await;
        assert_eq!(outcome, Outcome::ConnectionLost);
    }

    #[tokio::test]
    async fn notifications_do_not_stop_us() {
        let (tx, rx) = mpsc::channel::<Event>(8);
        tx.send(Event::Connected {
            granted_scopes: vec![],
            api_version: 1,
        })
        .await
        .unwrap();
        tx.send(Event::Notification {
            method: "session.changed".into(),
            params: serde_json::Value::Null,
        })
        .await
        .unwrap();
        tx.send(Event::Disconnected).await.unwrap();
        let outcome = run(rx, std::future::pending::<()>()).await;
        assert_eq!(outcome, Outcome::ConnectionLost);
    }

    #[tokio::test]
    async fn the_client_ending_stops_us() {
        let (tx, rx) = mpsc::channel::<Event>(8);
        drop(tx); // no Client left: the channel closes.
        let outcome = run(rx, std::future::pending::<()>()).await;
        assert_eq!(outcome, Outcome::ClientEnded);
    }
}
