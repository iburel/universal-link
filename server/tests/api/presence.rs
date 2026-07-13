// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Presence: `presence.update`, broadcast of `device.updated` / `device.online` /
//! `device.offline`, connection replacement and heartbeat
//! (doc/server-api.md, "Transport" and "Notifications").

use std::time::Duration;

use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use crate::support::*;

#[tokio::test]
async fn presence_update_status_broadcasts() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "pc-a", "linux").await;
    let mut b = online_device(&env, "alice", "pc-b", "macos").await;
    a.conn.drain().await;
    b.conn.drain().await;

    b.conn
        .request("presence.update", json!({ "status": "busy" }))
        .await
        .expect("presence.update");

    let params = a.conn.expect_notification("device.updated").await;
    assert_eq!(params["device"]["device_id"], b.device_id);
    assert_eq!(params["device"]["status"], "busy");

    // The requester has the response: it is not notified of its own change.
    b.conn.assert_silent().await;

    let list = a
        .conn
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(find_device(&list, &b.device_id)["status"], "busy");
}

#[tokio::test]
async fn presence_update_relay_url_broadcasts() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "pc-a", "linux").await;
    let mut b = online_device(&env, "alice", "pc-b", "macos").await;
    a.conn.drain().await;
    b.conn.drain().await;

    let relay_url = "https://relay-2.example/";
    b.conn
        .request("presence.update", json!({ "relay_url": relay_url }))
        .await
        .expect("presence.update");

    let params = a.conn.expect_notification("device.updated").await;
    assert_eq!(params["device"]["device_id"], b.device_id);
    assert_eq!(params["device"]["relay_url"], relay_url);

    b.conn.assert_silent().await;

    let list = a
        .conn
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(find_device(&list, &b.device_id)["relay_url"], relay_url);
}

#[tokio::test]
async fn presence_update_attestation_broadcasts() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "pc-a", "linux").await;
    let mut b = online_device(&env, "alice", "pc-b", "macos").await;
    a.conn.drain().await;
    b.conn.drain().await;

    // An opaque blob to the server (C7): it carries and rebroadcasts it without
    // ever decoding it — it's the peer that will verify it under its account key.
    let attestation = "ab".repeat(64);
    b.conn
        .request("presence.update", json!({ "attestation": attestation }))
        .await
        .expect("presence.update");

    let params = a.conn.expect_notification("device.updated").await;
    assert_eq!(params["device"]["device_id"], b.device_id);
    assert_eq!(params["device"]["attestation"], attestation);

    b.conn.assert_silent().await;

    let list = a
        .conn
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(find_device(&list, &b.device_id)["attestation"], attestation);
}

#[tokio::test]
async fn offline_keeps_attestation() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "pc-a", "linux").await;
    let b = online_device(&env, "alice", "pc-b", "macos").await;
    let mut b_conn = b.conn;
    a.conn.drain().await;
    b_conn.drain().await;

    let attestation = "cd".repeat(64);
    b_conn
        .request("presence.update", json!({ "attestation": attestation }))
        .await
        .expect("presence.update");
    a.conn.expect_notification("device.updated").await;

    // Unlike relay_url, the attestation is bound to the node_id (stable): it
    // SURVIVES going offline. Otherwise a peer would refuse the device on the
    // bounce, in the window before it has republished.
    drop(b_conn);
    a.conn.expect_notification("device.offline").await;

    let list = a
        .conn
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let record = find_device(&list, &b.device_id);
    assert_eq!(record["online"], false);
    assert_eq!(
        record["attestation"], attestation,
        "attestation lost when going offline"
    );
}

#[tokio::test]
async fn disconnect_broadcasts_offline() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "pc-a", "linux").await;
    let b = online_device(&env, "alice", "pc-b", "macos").await;
    a.conn.drain().await;

    // The connection is the presence: socket closed = offline.
    drop(b.conn);

    let params = a.conn.expect_notification("device.offline").await;
    assert_eq!(params["device_id"], b.device_id);
    assert_rfc3339(&params["last_seen"]);

    let list = a
        .conn
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let record = find_device(&list, &b.device_id);
    assert_eq!(record["online"], false);
    assert_rfc3339(&record["last_seen"]);
}

#[tokio::test]
async fn offline_clears_relay_url() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "pc-a", "linux").await;
    let b = online_device(&env, "alice", "pc-b", "macos").await;
    let mut b_conn = b.conn;
    a.conn.drain().await;
    b_conn.drain().await;

    b_conn
        .request(
            "presence.update",
            json!({ "relay_url": "https://relay-1.example/" }),
        )
        .await
        .expect("presence.update");
    a.conn.expect_notification("device.updated").await;

    // The dial info dies with the connection: a relay_url from the previous
    // session must not be served again as current.
    drop(b_conn);
    a.conn.expect_notification("device.offline").await;

    let list = a
        .conn
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let record = find_device(&list, &b.device_id);
    assert_eq!(record["online"], false);
    assert_eq!(
        record["relay_url"],
        json!(null),
        "previous session's relay_url still served after going offline"
    );

    // On reconnect, device.online does not serve the stale relay again either —
    // the device will republish a fresh one via presence.update.
    let mut conn2 = env.connect().await;
    authenticate(&mut conn2, &b.key, &b.device_id).await;
    let params = a.conn.expect_notification("device.online").await;
    assert_eq!(params["device"]["device_id"], b.device_id);
    assert_eq!(params["device"]["relay_url"], json!(null));

    let list = conn2
        .request("devices.list", json!({}))
        .await
        .expect("devices.list on the new connection");
    assert_eq!(find_device(&list, &b.device_id)["relay_url"], json!(null));
}

#[tokio::test]
async fn new_connection_replaces_old() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "pc-a", "linux").await;
    let mut b = online_device(&env, "alice", "pc-b", "macos").await;
    a.conn.drain().await;

    let mut conn2 = reconnect(&env, &b).await;

    // One device = at most one connection: the old one is closed.
    b.conn.expect_close().await;

    // The others see a plain device.online — a replacement must not produce an
    // offline/online flap.
    let params = a.conn.expect_notification("device.online").await;
    assert_eq!(params["device"]["device_id"], b.device_id);
    a.conn.assert_silent().await;

    let list = conn2
        .request("devices.list", json!({}))
        .await
        .expect("devices.list on the new connection");
    assert_eq!(find_device(&list, &b.device_id)["online"], true);
}

#[tokio::test]
async fn heartbeat_pings_are_sent() {
    let env = TestEnv::start_with(|c| c.heartbeat_interval = Duration::from_millis(200)).await;
    let mut device = online_device(&env, "alice", "pc-a", "linux").await;

    let mut got_ping = false;
    for _ in 0..20 {
        if matches!(device.conn.recv_frame().await, Message::Ping(_)) {
            got_ping = true;
            break;
        }
    }
    assert!(got_ping, "no ping received on an authenticated connection");
}

#[tokio::test]
async fn heartbeat_loss_marks_offline() {
    let env = TestEnv::start_with(|c| {
        c.heartbeat_interval = Duration::from_millis(100);
        c.heartbeat_max_missed = 2;
    })
    .await;
    let mut a = online_device(&env, "alice", "pc-a", "linux").await;
    let b = online_device(&env, "alice", "pc-b", "macos").await;

    // No drain() here: the offline lands in ~200-300 ms, during the window that
    // drain would absorb; wait_notification ignores the setup noise.
    // B keeps its socket open but never reads it again: without reads, no
    // automatic pong → heartbeat lost → offline.
    let params = a.conn.wait_notification("device.offline").await;
    assert_eq!(params["device_id"], b.device_id);
    assert_rfc3339(&params["last_seen"]);
}
