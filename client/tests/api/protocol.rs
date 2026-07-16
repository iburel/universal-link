// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The client's protocol conformance, observed from a scripted Core:
//! incoming requests, invalid frames.

use serde_json::json;
use universallink_ipc_client::Event;

use crate::connection::client_config_at;
use crate::support::*;

#[tokio::test]
async fn incoming_request_gets_method_not_found() {
    let mut scripted = ScriptedCore::start().await;
    let (_client, mut events) = universallink_ipc_client::spawn(client_config_at(scripted.path()));
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;

    // The Core calls the component (clipboard.get_data will come): the v1
    // client serves nothing, but replies cleanly — the connection survives.
    conn.send(&json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "clipboard.get_data",
        "params": { "tx_id": "x", "format": "text", "channel_token": "t" },
    }))
    .await;
    let v = conn.recv().await;
    assert_eq!(v["id"], 42);
    assert_eq!(v["error"]["code"], -32601);

    // The connection survived: a notification still goes through.
    conn.send(
        &json!({ "jsonrpc": "2.0", "method": "session.changed", "params": { "logged_in": false } }),
    )
    .await;
    let (method, _) = expect_notification(&mut events).await;
    assert_eq!(method, "session.changed");
}

#[tokio::test]
async fn served_method_surfaces_as_request_and_is_answered() {
    let mut scripted = ScriptedCore::start().await;
    let mut cfg = client_config_at(scripted.path());
    cfg.served_methods = vec!["clipboard.get_data".into()];
    let (client, mut events) = universallink_ipc_client::spawn(cfg);
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;

    // A served Core→component request is surfaced (not auto-refused).
    conn.send(&json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "clipboard.get_data",
        "params": { "tx_id": "x", "format": "text", "channel_token": "t" },
    }))
    .await;
    let (id, params) = match next_event(&mut events).await {
        Event::Request { id, method, params } => {
            assert_eq!(method, "clipboard.get_data");
            (id, params)
        }
        other => panic!("expected an incoming request, got {other:?}"),
    };
    assert_eq!(params["tx_id"], "x");

    // The component answers with the id carried by the event.
    client.respond(id, json!({})).await.expect("respond");
    let resp = conn.recv().await;
    assert_eq!(resp["id"], 42);
    assert_eq!(resp["result"], json!({}));

    // A method NOT in served_methods is still auto-refused; the connection lives.
    conn.send(&json!({ "jsonrpc": "2.0", "id": 43, "method": "other.method", "params": {} }))
        .await;
    let refusal = conn.recv().await;
    assert_eq!(refusal["id"], 43);
    assert_eq!(refusal["error"]["code"], -32601);
}

#[tokio::test]
async fn respond_error_carries_the_application_code() {
    let mut scripted = ScriptedCore::start().await;
    let mut cfg = client_config_at(scripted.path());
    cfg.served_methods = vec!["clipboard.get_data".into()];
    let (client, mut events) = universallink_ipc_client::spawn(cfg);
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;

    conn.send(&json!({ "jsonrpc": "2.0", "id": 7, "method": "clipboard.get_data", "params": {} }))
        .await;
    let id = match next_event(&mut events).await {
        Event::Request { id, .. } => id,
        other => panic!("expected an incoming request, got {other:?}"),
    };
    client.respond_error(id, "CLIP_STALE").await.expect("respond");
    let resp = conn.recv().await;
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["error"]["data"]["code"], "CLIP_STALE");
}

#[tokio::test]
async fn respond_after_reconnect_is_disconnected() {
    let mut scripted = ScriptedCore::start().await;
    let mut cfg = client_config_at(scripted.path());
    cfg.served_methods = vec!["clipboard.get_data".into()];
    let (client, mut events) = universallink_ipc_client::spawn(cfg);
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;

    conn.send(&json!({ "jsonrpc": "2.0", "id": 99, "method": "clipboard.get_data", "params": {} }))
        .await;
    let id = match next_event(&mut events).await {
        Event::Request { id, .. } => id,
        other => panic!("expected an incoming request, got {other:?}"),
    };

    // The delivering connection dies; the client reconnects (new generation).
    conn.send_raw(b"garbage without a colon\r\n\r\n").await;
    expect_disconnected(&mut events).await;
    let mut conn2 = scripted.accept().await;
    conn2.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;

    // The stale id must NOT be written onto the fresh connection.
    match client.respond(id, json!({})).await {
        Err(universallink_ipc_client::RequestError::Disconnected) => {}
        other => panic!("expected Disconnected, got {other:?}"),
    }
    conn2.assert_no_frame().await;
}

#[tokio::test]
async fn establishment_messages_are_buffered_and_served() {
    let mut scripted = ScriptedCore::start().await;
    let (_client, mut events) = universallink_ipc_client::spawn(client_config_at(scripted.path()));
    let mut conn = scripted.accept().await;
    let hello = conn.recv().await;

    // Before replying to the hello: two notifications and one incoming
    // request. The notifications wait for Connected (delivered afterward, in
    // order); the incoming request is turned away immediately.
    conn.send(&json!({ "jsonrpc": "2.0", "method": "session.changed", "params": { "seq": 1 } }))
        .await;
    conn.send(&json!({ "jsonrpc": "2.0", "id": 7, "method": "clipboard.get_data", "params": {} }))
        .await;
    conn.send(&json!({ "jsonrpc": "2.0", "method": "device.added", "params": { "seq": 2 } }))
        .await;
    let refusal = conn.recv().await;
    assert_eq!(refusal["id"], 7);
    assert_eq!(refusal["error"]["code"], -32601);

    conn.send(&json!({
        "jsonrpc": "2.0",
        "id": hello["id"],
        "result": { "status": "ok", "granted_scopes": hello["params"]["scopes"], "api_version": 1 },
    }))
    .await;
    expect_connected(&mut events, &["session.read"]).await;
    let (m, p) = expect_notification(&mut events).await;
    assert_eq!(
        (m.as_str(), p["seq"].as_i64()),
        ("session.changed", Some(1))
    );
    let (m, p) = expect_notification(&mut events).await;
    assert_eq!((m.as_str(), p["seq"].as_i64()), ("device.added", Some(2)));
}

#[tokio::test]
async fn invalid_frame_causes_reconnect() {
    let mut scripted = ScriptedCore::start().await;
    let (_client, mut events) = universallink_ipc_client::spawn(client_config_at(scripted.path()));
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;

    // Invalid frame (header line without a colon): fail-closed, the
    // client drops the connection and restarts the cycle.
    conn.send_raw(b"garbage without a colon\r\n\r\n").await;
    expect_disconnected(&mut events).await;
    conn.expect_close().await;

    let mut conn2 = scripted.accept().await;
    conn2.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;
}
