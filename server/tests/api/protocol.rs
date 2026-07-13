// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Protocol conformance: HTTP `/health`, JSON-RPC 2.0 (standard codes), rate
//! limiting and notification shape (doc/server-api.md).

use crate::support::*;
use serde_json::{Value, json};

#[tokio::test]
async fn health_endpoint() {
    let env = TestEnv::start().await;
    assert_eq!(env.http_get_status("/health").await, 200);
}

#[tokio::test]
async fn unknown_method() {
    let env = TestEnv::start().await;
    let mut device = online_device(&env, "alice", "Desktop-PC", "linux").await;

    let err = device
        .conn
        .request("no.such_method", json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32601);
}

#[tokio::test]
async fn parse_error() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;

    conn.send_raw("{ not json").await;
    let v = conn.recv_raw_json().await;
    assert_eq!(v["error"]["code"], -32700);
    // JSON-RPC 2.0: id undeterminable → the response carries `id: null`.
    assert_eq!(v.get("id"), Some(&Value::Null));
}

#[tokio::test]
async fn invalid_params() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;

    let err = conn.request("auth.enroll", json!({})).await.unwrap_err();
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn rate_limited() {
    let env = TestEnv::start_with(|c| c.max_requests_per_minute = Some(5)).await;
    let mut conn = env.connect().await;

    let mut limited = false;
    for _ in 0..20 {
        if let Err(err) = conn.request("auth.challenge", json!({})).await {
            assert_eq!(err.app_code(), "RATE_LIMITED");
            limited = true;
        }
    }
    assert!(
        limited,
        "20 requests with a limit of 5/min: at least one should have been RATE_LIMITED"
    );
}

#[tokio::test]
async fn unknown_fields_are_ignored() {
    let env = TestEnv::start().await;
    let mut device = online_device(&env, "alice", "Desktop-PC", "linux").await;

    // Tolerant JSON (spec, Versioning): an unknown field in the params is not an
    // error — a prerequisite for additive extensions.
    let result = device
        .conn
        .request("auth.challenge", json!({ "future_field": true }))
        .await
        .expect("auth.challenge with an unknown field");
    assert!(result["nonce"].is_string());

    device
        .conn
        .request(
            "presence.update",
            json!({ "status": "busy", "future_field": { "x": 1 } }),
        )
        .await
        .expect("presence.update with an unknown field");
}

#[tokio::test]
async fn notifications_are_wellformed_jsonrpc() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "PC-A", "linux").await;
    let mut b = online_device(&env, "alice", "PC-B", "macos").await;
    a.conn.drain().await;

    b.conn
        .request("presence.update", json!({ "status": "busy" }))
        .await
        .expect("presence.update");

    // Raw frame on the observer side: a JSON-RPC 2.0 notification, so no `id`.
    let frame = a.conn.recv_raw_json().await;
    assert_eq!(frame["jsonrpc"], "2.0");
    assert_eq!(frame["method"], "device.updated");
    assert!(
        frame["params"].is_object(),
        "params must be an object: {frame}"
    );
    assert!(frame.get("id").is_none(), "no id key expected: {frame}");
}
