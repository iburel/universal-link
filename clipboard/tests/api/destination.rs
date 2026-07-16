// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Destination side and resync against a SCRIPTED Core. `clipboard.remote_updated`
//! needs a peer, which a lone real Core cannot produce; the scripted server
//! drives the exchange deterministically (the same technique the client crate
//! uses). The consumer-channel byte transport itself is proven against a real
//! Core in the client crate's `channel` suite — here we exercise the
//! orchestrator's promise / paste / pull orchestration on top of it.

use serde_json::json;
use tokio::sync::mpsc;
use universallink_clipboard::{BackendEvent, run};

use crate::support::*;

/// Boots the orchestrator against a fresh scripted Core, walks the connect
/// handshake, and answers the resync `clipboard.current` with "no live clip".
/// Returns the live control connection, the backend-event sender, the fake
/// backend, and the scripted Core (for the consumer-channel connection).
async fn scripted_orchestrator() -> (
    ScriptedCore,
    ScriptedConn,
    mpsc::Sender<BackendEvent>,
    FakeBackend,
) {
    let mut scripted = ScriptedCore::start().await;
    let fake = FakeBackend::default();
    let (backend_tx, backend_rx) = mpsc::channel(16);
    let (client, events) = spawn_client(
        &scripted.path(),
        "spawn-token".into(),
        "clipboard-backend",
        &BACKEND_SCOPES,
        &["clipboard.get_data"],
    );
    tokio::spawn(run(
        client,
        events,
        fake.clone(),
        scripted.path(),
        backend_rx,
        never(),
    ));
    let mut conn = scripted.accept().await;
    conn.handle_hello().await;
    // Resync fires on connect; no live clip here.
    conn.handle_request("clipboard.current", json!({})).await;
    (scripted, conn, backend_tx, fake)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_copy_is_offered_to_the_os() {
    let (_scripted, mut conn, _backend_tx, fake) = scripted_orchestrator().await;

    conn.notify(
        "clipboard.remote_updated",
        json!({
            "device_id": "remote-1",
            "tx_id": "tx-remote",
            "formats": [{ "format": "text", "size": 11 }],
        }),
    )
    .await;

    let clip = fake.await_offer().await;
    assert_eq!(clip.tx_id, "tx-remote");
    assert_eq!(clip.formats.len(), 1);
    assert_eq!(clip.formats[0].id, "text");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paste_pulls_the_remote_blob_over_a_consumer_channel() {
    let (mut scripted, mut conn, backend_tx, fake) = scripted_orchestrator().await;

    conn.notify(
        "clipboard.remote_updated",
        json!({
            "device_id": "remote-1",
            "tx_id": "tx-remote",
            "formats": [{ "format": "text" }],
        }),
    )
    .await;
    fake.await_offer().await;

    // A local paste of the promised format.
    backend_tx
        .send(BackendEvent::Paste {
            format: "text".into(),
            token: 7,
        })
        .await
        .expect("send Paste");

    // The orchestrator opens the transaction, then a consumer channel.
    let params = conn
        .handle_request("transactions.open", json!({ "channel_token": "ct-1" }))
        .await;
    assert_eq!(params["tx_id"], "tx-remote");

    let mut channel = scripted.accept().await;
    assert_eq!(channel.recv_attach().await, "ct-1");
    let (tag, _payload) = channel.recv_binary().await;
    assert_eq!(tag, 0x02, "the consumer sends a FETCH frame");
    channel.send_data(0, b"remote-text").await;
    channel.send_eof().await;

    let (token, format, bytes) = fake.await_delivered().await;
    assert_eq!(token, 7);
    assert_eq!(format, "text");
    assert_eq!(bytes, b"remote-text");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paste_with_no_promise_fails_without_a_request() {
    let (_scripted, _conn, backend_tx, fake) = scripted_orchestrator().await;

    // No remote_updated was received: nothing is promised.
    backend_tx
        .send(BackendEvent::Paste {
            format: "text".into(),
            token: 9,
        })
        .await
        .expect("send Paste");

    let (token, format) = fake.await_failed().await;
    assert_eq!(token, 9);
    assert_eq!(format, "text");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tx_stale_at_open_refuses_the_paste() {
    let (_scripted, mut conn, backend_tx, fake) = scripted_orchestrator().await;

    conn.notify(
        "clipboard.remote_updated",
        json!({
            "device_id": "remote-1",
            "tx_id": "tx-remote",
            "formats": [{ "format": "text" }],
        }),
    )
    .await;
    fake.await_offer().await;

    backend_tx
        .send(BackendEvent::Paste {
            format: "text".into(),
            token: 3,
        })
        .await
        .expect("send Paste");

    // The source superseded the clip meanwhile: transactions.open → TX_STALE.
    conn.handle_request_error("transactions.open", "TX_STALE")
        .await;

    let (token, format) = fake.await_failed().await;
    assert_eq!(token, 3);
    assert_eq!(format, "text");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_remote_update_withdraws_the_promise() {
    let (_scripted, mut conn, _backend_tx, fake) = scripted_orchestrator().await;

    conn.notify(
        "clipboard.remote_updated",
        json!({
            "device_id": "remote-1",
            "tx_id": "tx-remote",
            "formats": [{ "format": "text" }],
        }),
    )
    .await;
    fake.await_offer().await;

    // The source cleared its clipboard: empty formats withdraw the promise.
    conn.notify(
        "clipboard.remote_updated",
        json!({ "device_id": "remote-1", "tx_id": "tx-empty", "formats": [] }),
    )
    .await;

    fake.await_release().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn resync_offers_a_remote_clip_that_is_not_our_own() {
    let mut scripted = ScriptedCore::start().await;
    let fake = FakeBackend::default();
    let (_backend_tx, backend_rx) = mpsc::channel(16);
    let (client, events) = spawn_client(
        &scripted.path(),
        "spawn-token".into(),
        "clipboard-backend",
        &BACKEND_SCOPES,
        &["clipboard.get_data"],
    );
    tokio::spawn(run(
        client,
        events,
        fake.clone(),
        scripted.path(),
        backend_rx,
        never(),
    ));

    let mut conn = scripted.accept().await;
    conn.handle_hello().await;

    // Resync: a remote device holds the live clip.
    conn.handle_request(
        "clipboard.current",
        json!({
            "device_id": "remote-9",
            "tx_id": "tx-live",
            "formats": [{ "format": "text" }],
        }),
    )
    .await;
    // The orchestrator resolves whether that device is us.
    conn.handle_request(
        "devices.list",
        json!([
            { "device_id": "me", "is_self": true },
            { "device_id": "remote-9", "is_self": false },
        ]),
    )
    .await;

    let clip = fake.await_offer().await;
    assert_eq!(clip.tx_id, "tx-live");
}
