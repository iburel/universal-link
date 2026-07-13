// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Protocol conformance: LSP framing, JSON-RPC 2.0 grammar, tolerance to
//! extensions (doc/core-api.md, "Principles" and "Versioning").
//!
//! The raw frames use ids ≥ 100 so as not to collide with those of
//! `TestComponent::request`'s counter.

use serde_json::{Value, json};

use crate::support::*;

#[tokio::test]
async fn unknown_method() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let err = c.request("no.such_method", json!({})).await.unwrap_err();
    assert_eq!(err.code, -32601);
}

#[tokio::test]
async fn parse_error() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    c.send_frame("{ not json").await;
    let v = c.recv_raw_json().await;
    assert_eq!(v["error"]["code"], -32700);
    // JSON-RPC 2.0: undeterminable id → the response carries `id: null`.
    assert_eq!(v.get("id"), Some(&Value::Null));
}

#[tokio::test]
async fn request_without_method_is_invalid() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    c.send_frame(r#"{"jsonrpc":"2.0","id":100,"params":{}}"#)
        .await;
    let v = c.recv_raw_json().await;
    assert_eq!(v["error"]["code"], -32600);
    assert_eq!(v["id"], 100);
}

#[tokio::test]
async fn missing_required_param_is_invalid_params() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    // hello without role or scopes.
    let err = c
        .request("hello", json!({ "name": "third-party", "version": "1" }))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn unknown_client_notification_is_ignored() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    // Without an id = notification: never a response, even an unknown one
    // (JSON-RPC 2.0), and the connection stays alive.
    c.send_frame(r#"{"jsonrpc":"2.0","method":"no.such_notification"}"#)
        .await;
    c.assert_silent().await;
    c.request("session.status", json!({}))
        .await
        .expect("the connection must survive an unknown notification");
}

#[tokio::test]
async fn pipelined_requests_are_answered_in_order() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    // Two frames in a single write: the framing must split them apart.
    let mut bytes = frame(r#"{"jsonrpc":"2.0","id":100,"method":"session.status","params":{}}"#);
    bytes.extend(frame(
        r#"{"jsonrpc":"2.0","id":101,"method":"session.status","params":{}}"#,
    ));
    c.send_bytes(&bytes).await;

    assert_eq!(c.recv_raw_json().await["id"], 100);
    assert_eq!(c.recv_raw_json().await["id"], 101);
}

#[tokio::test]
async fn unknown_headers_are_ignored() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let payload = r#"{"jsonrpc":"2.0","id":100,"method":"session.status","params":{}}"#;
    let bytes = format!(
        "X-Future: yes\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
        payload.len(),
        payload
    );
    c.send_bytes(bytes.as_bytes()).await;
    assert_eq!(c.recv_raw_json().await["id"], 100);
}

#[tokio::test]
async fn content_length_is_case_insensitive() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let payload = r#"{"jsonrpc":"2.0","id":100,"method":"session.status","params":{}}"#;
    let bytes = format!("content-length: {}\r\n\r\n{}", payload.len(), payload);
    c.send_bytes(bytes.as_bytes()).await;
    assert_eq!(c.recv_raw_json().await["id"], 100);
}

#[tokio::test]
async fn oversized_content_length_closes_connection() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    // Preposterous announcement: the Core must close without allocating.
    c.send_bytes(b"Content-Length: 999999999\r\n\r\n").await;
    c.expect_close().await;
}

#[tokio::test]
async fn malformed_header_closes_connection() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    c.send_bytes(b"anything without a colon\r\n\r\n")
        .await;
    c.expect_close().await;
}

#[tokio::test]
async fn unbounded_header_section_closes_connection() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    // 16 KiB of well-formed but endless headers: the section is capped.
    let flood = "X-Noise: lots\r\n".repeat(1000);
    c.send_bytes(flood.as_bytes()).await;
    c.expect_close().await;
}

#[tokio::test]
async fn unknown_json_fields_are_ignored() {
    let core = TestCore::start().await;
    let token = core.mint("tray", &["session.read"]);
    let mut c = core.connect().await;

    // Tolerant JSON (spec, Versioning): an unknown field in the params is not
    // an error — a prerequisite for additive extensions.
    let r = c
        .request(
            "hello",
            json!({
                "name": "tray-official",
                "version": "0.0-test",
                "role": "tray",
                "scopes": ["session.read"],
                "token": token,
                "future_field": { "x": 1 },
            }),
        )
        .await
        .expect("hello with an unknown field");
    assert_eq!(r["status"], "ok");

    c.request(
        "events.subscribe",
        json!({ "topics": ["session"], "future_field": true }),
    )
    .await
    .expect("subscribe with an unknown field");
}
