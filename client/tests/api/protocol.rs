// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The client's protocol conformance, observed from a scripted Core:
//! incoming requests, invalid frames.

use serde_json::json;

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
        "params": { "clip_id": "x", "format": "text" },
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
