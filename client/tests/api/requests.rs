// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Requests: multiplexing, JSON-RPC error relaying, fail-closed while
//! offline, timeout, connection loss in flight.

use serde_json::json;
use universallink_ipc_client::RequestError;

use crate::connection::client_config_at;
use crate::support::*;

#[tokio::test]
async fn rpc_errors_are_relayed() {
    let core = TestCore::start().await;
    let (client, _events) = connected(&core, "gui", &["session.read", "devices.read"], &[]).await;

    // Application error: domain code in data.code, relayed as-is.
    let err = client.request("devices.list", json!({})).await.unwrap_err();
    match err {
        RequestError::Rpc(e) => {
            assert_eq!(e.data_code.as_deref(), Some("SERVER_UNREACHABLE"), "{e:?}");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Pure JSON-RPC error: unknown method, no application code.
    let err = client.request("nope.nope", json!({})).await.unwrap_err();
    match err {
        RequestError::Rpc(e) => {
            assert_eq!(e.code, -32601);
            assert_eq!(e.data_code, None);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Missing scope: SCOPE_DENIED.
    let err = client
        .request("devices.rename", json!({ "device_id": "d_x", "name": "X" }))
        .await
        .unwrap_err();
    match err {
        RequestError::Rpc(e) => assert_eq!(e.data_code.as_deref(), Some("SCOPE_DENIED")),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_requests_are_multiplexed() {
    let core = TestCore::start().await;
    let (client, _events) = connected(&core, "gui", &["session.read"], &[]).await;

    let (a, b, c) = tokio::join!(
        client.request("session.status", json!({})),
        client.request("nope.nope", json!({})),
        client.request("session.status", json!({})),
    );
    assert_eq!(a.expect("status a")["logged_in"], false);
    assert!(matches!(b.unwrap_err(), RequestError::Rpc(e) if e.code == -32601));
    assert_eq!(c.expect("status c")["logged_in"], false);
}

#[tokio::test]
async fn request_without_connection_fails_fast() {
    let mut core = TestCore::start().await;
    let (client, mut events) = connected(&core, "gui", &["session.read"], &[]).await;
    core.stop();
    expect_disconnected(&mut events).await;

    let start = std::time::Instant::now();
    let err = client
        .request("session.status", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, RequestError::NotConnected), "{err:?}");
    // Immediate: no waiting on a request timeout.
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "NotConnected must be immediate ({:?})",
        start.elapsed()
    );
}

#[tokio::test]
async fn inflight_request_fails_on_disconnect() {
    let mut scripted = ScriptedCore::start().await;
    let (client, mut events) = universallink_ipc_client::spawn(client_config_at(scripted.path()));
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;

    let pending = tokio::spawn({
        let client = client.clone();
        async move { client.request("session.status", json!({})).await }
    });
    // The scripted Core receives the request… and closes without replying.
    let v = conn.recv().await;
    assert_eq!(v["method"], "session.status");
    drop(conn);

    let err = pending.await.expect("request task").unwrap_err();
    assert!(matches!(err, RequestError::Disconnected), "{err:?}");
    expect_disconnected(&mut events).await;
}

#[tokio::test]
async fn slow_core_times_out() {
    let mut scripted = ScriptedCore::start().await;
    let mut config = client_config_at(scripted.path());
    config.request_timeout = std::time::Duration::from_millis(200);

    let (client, mut events) = universallink_ipc_client::spawn(config);
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;

    // Request received but never answered: timeout on the client side, the
    // connection survives (a slow response is not a dead connection).
    let pending = tokio::spawn({
        let client = client.clone();
        async move { client.request("session.status", json!({})).await }
    });
    let v = conn.recv().await;
    assert_eq!(v["method"], "session.status");

    let err = pending.await.expect("request task").unwrap_err();
    assert!(matches!(err, RequestError::Timeout), "{err:?}");
    assert_no_event(&mut events).await;

    // Late response: orphan, ignored — the connection survives and a
    // new request succeeds.
    conn.send(&json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "logged_in": false } }))
        .await;
    conn.send(&json!({ "jsonrpc": "2.0", "method": "session.changed", "params": {} }))
        .await;
    let (m, _) = expect_notification(&mut events).await;
    assert_eq!(m, "session.changed");
    let pending = tokio::spawn({
        let client = client.clone();
        async move { client.request("session.status", json!({})).await }
    });
    let v2 = conn.recv().await;
    conn.send(&json!({ "jsonrpc": "2.0", "id": v2["id"], "result": { "ok": true } }))
        .await;
    let r = pending.await.expect("request task").expect("response");
    assert_eq!(r["ok"], true);
}

#[tokio::test]
async fn request_times_out_even_when_the_manager_is_stuck() {
    let mut scripted = ScriptedCore::start().await;
    let mut config = client_config_at(scripted.path());
    config.request_timeout = std::time::Duration::from_millis(300);

    let (client, mut events) = universallink_ipc_client::spawn(config);
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;

    // The consumer keeps the event channel but stops reading it; the
    // Core floods notifications until it suspends the manager under
    // backpressure (event channel full + read channel full).
    for i in 0..400 {
        conn.send(&json!({ "jsonrpc": "2.0", "method": "flood", "params": { "i": i } }))
            .await;
    }

    // All requests — including those whose command can no longer even
    // be enqueued — return Timeout in time: none hangs without bound.
    let tasks: Vec<_> = (0..80)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.request("session.status", json!({})).await })
        })
        .collect();
    for t in tasks {
        let err = tokio::time::timeout(std::time::Duration::from_secs(3), t)
            .await
            .expect("request hung without bound")
            .expect("request task")
            .unwrap_err();
        assert!(matches!(err, RequestError::Timeout), "{err:?}");
    }
}
