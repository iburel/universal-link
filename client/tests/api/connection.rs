// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Managed connection cycle: establishment, file token re-read, backoff,
//! reconnection after a Core restart, version incompatibility.

use std::time::{Duration, Instant};

use serde_json::json;
use universallink_ipc_client::{Event, RequestError, TokenSource};

use crate::support::*;

#[tokio::test]
async fn connect_and_hello_reports_scopes_and_version() {
    let core = TestCore::start().await;
    let (client, _events) = connected(&core, "gui", &["session.read"], &[]).await;

    // The connection is active: a request goes through.
    let r = client
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["logged_in"], false);
}

#[tokio::test]
async fn client_waits_for_core_and_rereads_token() {
    let mut core = TestCore::start().await;
    let stale_token = core.file_token();
    core.stop();

    // Client started with the Core absent: no Connected, it waits in backoff.
    let (client, mut events) =
        universallink_ipc_client::spawn(client_config(&core, "gui", &["session.read"], &[]));
    assert_no_event(&mut events).await;

    // The Core (re)starts: new token on disk. The client must connect
    // with the FRESH token — the one read before the restart is dead.
    core.restart().await;
    assert_ne!(core.file_token(), stale_token, "token regenerated");
    expect_connected(&mut events, &["session.read"]).await;
    client
        .request("session.status", json!({}))
        .await
        .expect("session.status after a late connection");
}

#[tokio::test]
async fn reconnects_after_core_restart() {
    let mut core = TestCore::start().await;
    let (client, mut events) = connected(&core, "gui", &["session.read"], &[]).await;

    core.restart().await;
    expect_disconnected(&mut events).await;
    expect_connected(&mut events, &["session.read"]).await;

    let r = client
        .request("session.status", json!({}))
        .await
        .expect("session.status after reconnection");
    assert_eq!(r["logged_in"], false);
}

#[tokio::test]
async fn spawn_token_connects() {
    let core = TestCore::start().await;
    let token = core.mint("tray", &["session.read", "devices.read"]);
    let mut config = client_config(&core, "tray", &["session.read", "devices.read"], &[]);
    config.token = TokenSource::Spawn(token);

    let (client, mut events) = universallink_ipc_client::spawn(config);
    expect_connected(&mut events, &["session.read", "devices.read"]).await;
    client
        .request("session.status", json!({}))
        .await
        .expect("session.status with spawn token");
}

#[tokio::test]
async fn invalid_token_never_connects() {
    let core = TestCore::start().await;
    let mut config = client_config(&core, "tray", &["session.read"], &[]);
    config.token = TokenSource::Spawn("deadbeef".into());

    let (client, mut events) = universallink_ipc_client::spawn(config);
    // INVALID_TOKEN on every attempt: never Connected, the client loops.
    assert_no_event(&mut events).await;
    // And requests fail immediately, fail-closed.
    let err = client
        .request("session.status", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, RequestError::NotConnected), "{err:?}");
}

#[tokio::test]
async fn incompatible_api_version_is_terminal() {
    let mut scripted = ScriptedCore::start().await;
    let mut config = client_config_at(scripted.path());
    config.topics = vec![];

    let (client, mut events) = universallink_ipc_client::spawn(config);
    let mut conn = scripted.accept().await;
    conn.handle_hello(2).await;

    match next_event(&mut events).await {
        Event::Incompatible { api_version } => assert_eq!(api_version, 2),
        other => panic!("unexpected event: {other:?}"),
    }
    // Permanent shutdown: no reconnection, requests fail immediately.
    scripted.assert_no_connection().await;
    let err = client
        .request("session.status", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, RequestError::NotConnected), "{err:?}");
}

#[tokio::test]
async fn request_during_establishment_fails_fast() {
    let mut scripted = ScriptedCore::start().await;
    let (client, mut events) = universallink_ipc_client::spawn(client_config_at(scripted.path()));
    let mut conn = scripted.accept().await;
    // hello received but left unanswered: establishment is in progress.
    let hello = conn.recv().await;
    assert_eq!(hello["method"], "hello");

    // Request during the window: immediate NotConnected, no queue.
    let start = Instant::now();
    let err = client
        .request("session.status", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, RequestError::NotConnected), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "NotConnected must be immediate ({:?})",
        start.elapsed()
    );

    // Establishment then succeeds; the failed request does NOT replay —
    // the first frame after connection is the fresh request.
    conn.send(&json!({
        "jsonrpc": "2.0",
        "id": hello["id"],
        "result": { "status": "ok", "granted_scopes": hello["params"]["scopes"], "api_version": 1 },
    }))
    .await;
    expect_connected(&mut events, &["session.read"]).await;
    let pending = tokio::spawn({
        let client = client.clone();
        async move { client.request("fresh.method", json!({})).await }
    });
    let v = conn.recv().await;
    assert_eq!(
        v["method"], "fresh.method",
        "the request issued while offline must not replay"
    );
    conn.send(&json!({ "jsonrpc": "2.0", "id": v["id"], "result": {} }))
        .await;
    pending.await.expect("request task").expect("response");
}

#[tokio::test]
async fn hello_pending_is_a_cycle_failure() {
    let mut scripted = ScriptedCore::start().await;
    let (_client, mut events) = universallink_ipc_client::spawn(client_config_at(scripted.path()));
    let mut conn = scripted.accept().await;
    let hello = conn.recv().await;
    // Interactive third-party enrollment is not supported in v1: for an
    // official component, `pending` = missing token = cycle failure.
    conn.send(&json!({ "jsonrpc": "2.0", "id": hello["id"], "result": { "status": "pending" } }))
        .await;
    // Never Connected: the client closes and retries.
    conn.expect_close().await;
    let mut retry = scripted.accept().await;
    retry.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;
}

#[tokio::test]
async fn backoff_doubles_and_resets() {
    let mut scripted = ScriptedCore::start().await;
    let (_client, mut events) = universallink_ipc_client::spawn(client_config_at(scripted.path()));

    // 4 rejected hellos: the attempts space out by doubling (base 25 ms).
    // `sleep` guarantees a minimum, never a maximum: we only assert lower
    // bounds — insensitive to CI load.
    let mut stamps = Vec::new();
    for _ in 0..4 {
        let mut conn = scripted.accept().await;
        stamps.push(Instant::now());
        let hello = conn.recv().await;
        conn.send(&json!({
            "jsonrpc": "2.0",
            "id": hello["id"],
            "error": { "code": -32000, "message": "no" },
        }))
        .await;
    }
    let mut conn = scripted.accept().await;
    stamps.push(Instant::now());
    assert!(
        stamps[3] - stamps[2] >= Duration::from_millis(100),
        "3rd wait too short: {:?}",
        stamps[3] - stamps[2]
    );
    assert!(
        stamps[4] - stamps[3] >= Duration::from_millis(200),
        "4th wait too short: {:?}",
        stamps[4] - stamps[3]
    );

    // Successful establishment: the delay returns to the BASE. An immediate
    // outage must be retried in ~25 ms — not the 400 ms of the inflated
    // delay that would remain if the reset were missing.
    conn.handle_hello(1).await;
    expect_connected(&mut events, &["session.read"]).await;
    drop(conn);
    expect_disconnected(&mut events).await;
    let lost = Instant::now();
    let _retry = scripted.accept().await;
    assert!(
        lost.elapsed() < Duration::from_millis(300),
        "backoff not reset to the base after a success: {:?}",
        lost.elapsed()
    );
}

// One subscription for the lot: a Core that knows every topic must not be asked
// twice — the second call would be a round trip on every reconnection.
#[tokio::test]
async fn the_topics_are_all_asked_for_in_one_call() {
    let mut scripted = ScriptedCore::start().await;
    let mut config = client_config_at(scripted.path());
    config.topics = vec!["session".into()];
    config.optional_topics = vec!["pairing".into()];

    let (_client, mut events) = universallink_ipc_client::spawn(config);
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;

    let sub = conn.recv().await;
    assert_eq!(sub["method"], "events.subscribe");
    assert_eq!(sub["params"]["topics"], json!(["session", "pairing"]));
    conn.send(&json!({ "jsonrpc": "2.0", "id": sub["id"], "result": {} }))
        .await;

    expect_connected(&mut events, &["session.read"]).await;
    conn.assert_no_frame().await;
}

// A Core older than an optional topic refuses the WHOLE list (it subscribes all
// or nothing). The connection must survive that: without the fallback, an
// interface that has grown a topic would never connect to a Core that is merely
// still running from the previous session.
#[tokio::test]
async fn an_optional_topic_the_core_does_not_know_costs_only_that_topic() {
    let mut scripted = ScriptedCore::start().await;
    let mut config = client_config_at(scripted.path());
    config.topics = vec!["session".into()];
    config.optional_topics = vec!["pairing".into()];

    let (_client, mut events) = universallink_ipc_client::spawn(config);
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;

    let both = conn.recv().await;
    assert_eq!(both["params"]["topics"], json!(["session", "pairing"]));
    conn.send(&json!({
        "jsonrpc": "2.0",
        "id": both["id"],
        "error": { "code": -32602, "message": "invalid params: topics" },
    }))
    .await;

    // Second call, on the SAME connection: the required topics alone.
    let required = conn.recv().await;
    assert_eq!(required["method"], "events.subscribe");
    assert_eq!(required["params"]["topics"], json!(["session"]));
    conn.send(&json!({ "jsonrpc": "2.0", "id": required["id"], "result": {} }))
        .await;

    expect_connected(&mut events, &["session.read"]).await;
}

// The fallback is for the OPTIONAL half only: a required topic that is refused is
// still a cycle failure. Nothing is downgraded silently.
#[tokio::test]
async fn a_required_topic_the_core_refuses_still_fails_the_cycle() {
    let mut scripted = ScriptedCore::start().await;
    let mut config = client_config_at(scripted.path());
    config.topics = vec!["session".into()];

    let (_client, mut events) = universallink_ipc_client::spawn(config);
    let mut conn = scripted.accept().await;
    conn.handle_hello(1).await;

    let sub = conn.recv().await;
    assert_eq!(sub["params"]["topics"], json!(["session"]));
    conn.send(&json!({
        "jsonrpc": "2.0",
        "id": sub["id"],
        "error": { "code": -32602, "message": "invalid params: topics" },
    }))
    .await;

    // No second attempt, and never Connected: the client closes and retries.
    conn.expect_close().await;
    assert_no_event(&mut events).await;
}

/// Config pointed at a scripted Core (static token, no file).
pub fn client_config_at(path: std::path::PathBuf) -> universallink_ipc_client::ClientConfig {
    universallink_ipc_client::ClientConfig {
        ipc_path: path,
        token: TokenSource::Spawn("scripted-token".into()),
        name: "client-test".into(),
        version: "0.0-test".into(),
        role: "gui".into(),
        scopes: vec!["session.read".into()],
        topics: vec![],
        optional_topics: Vec::new(),
        served_methods: vec![],
        reconnect_base_delay: std::time::Duration::from_millis(25),
        request_timeout: RESPONSE_TIMEOUT,
    }
}
