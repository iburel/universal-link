// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Peer messages between same-role components (doc/core-api.md, "peers.*";
//! ticket #83).
//!
//! `peers.send { device_id, payload }` carries an opaque, capped payload to
//! the component holding the SAME role on another attested device of the
//! account; it arrives as the subscription-less `peer.message { device_id,
//! payload }` push, the sender named by the receiver's own directory. The
//! sender's role is stamped by its Core, never chosen. One bounded round-trip:
//! `DEVICE_UNKNOWN` / `DEVICE_OFFLINE` follow `files.send`'s doctrine,
//! `COMPONENT_ABSENT` is the remote Core's word, `PAYLOAD_TOO_LARGE` is the
//! cap's own code at the RPC boundary.

use onedevice_core::PeerTransport;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::clipboard_net::RawPeer;
use crate::support::*;

/// A messaging component: the `peers.message` scope on a given role, plus what
/// the cross-Core waits need.
async fn messenger(core: &TestCore, name: &str, role: &str) -> TestComponent {
    spawn_component(
        core,
        name,
        role,
        &["peers.message", "devices.read", "session.read"],
    )
    .await
}

/// A connected pair with a `custom`-role messenger on each side.
async fn messaging_pair(server: &TestServer) -> (TestCore, TestComponent, TestCore, TestComponent) {
    let (a, b) = TestCore::start_pair(server).await;
    let mut ma = messenger(&a, "engine", "custom").await;
    let mut mb = messenger(&b, "engine", "custom").await;
    wait_server_connected(&mut ma, true).await;
    wait_server_connected(&mut mb, true).await;
    wait_reachable(&mut ma, b.device_id()).await;
    wait_reachable(&mut mb, a.device_id()).await;
    (a, ma, b, mb)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_message_lands_on_the_same_role_component() {
    let server = TestServer::start().await;
    let (a, mut ma, _b, mut mb) = messaging_pair(&server).await;

    let payload = json!({ "kind": "digest", "entries": [1, 2, 3] });
    ma.request(
        "peers.send",
        json!({ "device_id": _b.device_id(), "payload": payload }),
    )
    .await
    .expect("peers.send");

    // Arrival: the opaque payload verbatim, the sender named by OUR directory.
    let msg = mb.wait_notification("peer.message").await;
    assert_eq!(msg["device_id"], json!(a.device_id()));
    assert_eq!(msg["payload"], payload);
}

#[tokio::test(flavor = "multi_thread")]
async fn delivery_is_role_to_same_role_only() {
    let server = TestServer::start().await;
    let (_a, mut ma, b, mut mb) = messaging_pair(&server).await;
    // A DIFFERENT role holding the very same scope on B: never a recipient.
    let mut tray = messenger(&b, "tray", "tray").await;

    ma.request(
        "peers.send",
        json!({ "device_id": b.device_id(), "payload": { "n": 1 } }),
    )
    .await
    .expect("peers.send");

    mb.wait_notification("peer.message").await;
    tray.assert_silent().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_component_of_the_role_receives() {
    let server = TestServer::start().await;
    let (_a, mut ma, b, mut mb) = messaging_pair(&server).await;
    let mut second = messenger(&b, "engine-bis", "custom").await;

    ma.request(
        "peers.send",
        json!({ "device_id": b.device_id(), "payload": "fan-out" }),
    )
    .await
    .expect("peers.send");

    assert_eq!(
        mb.wait_notification("peer.message").await["payload"],
        json!("fan-out")
    );
    assert_eq!(
        second.wait_notification("peer.message").await["payload"],
        json!("fan-out")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn component_absent_is_the_remote_cores_word() {
    let server = TestServer::start().await;
    let (_a, mut ma, b, mb) = messaging_pair(&server).await;

    // The role is held remotely but WITHOUT the receiving scope: absent.
    drop(mb);
    let _bare = spawn_component(&b, "bare", "custom", &["devices.read"]).await;
    let err = ma
        .request(
            "peers.send",
            json!({ "device_id": b.device_id(), "payload": {} }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "COMPONENT_ABSENT");
}

#[tokio::test(flavor = "multi_thread")]
async fn send_refuses_bad_targets_scopes_and_oversize() {
    let server = TestServer::start().await;
    let (a, mut ma, _b, _mb) = messaging_pair(&server).await;

    // Unknown device: fail-closed, indistinguishable from unattested.
    let err = ma
        .request(
            "peers.send",
            json!({ "device_id": "d_nobody", "payload": {} }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");

    // One's own device: a loopback nothing needs.
    let err = ma
        .request(
            "peers.send",
            json!({ "device_id": a.device_id(), "payload": {} }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);

    // A missing payload is not an empty one.
    let err = ma
        .request("peers.send", json!({ "device_id": "d_x" }))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);

    // The cap speaks its own code, before any dial.
    let big = "x".repeat(70 * 1024);
    let err = ma
        .request("peers.send", json!({ "device_id": "d_x", "payload": big }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "PAYLOAD_TOO_LARGE");

    // And the scope guards the door.
    let mut bare = spawn_component(&a, "bare", "custom", &["devices.read"]).await;
    let err = bare
        .request("peers.send", json!({ "device_id": "d_x", "payload": {} }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

// ---------------------------------------------------------------------------
// The wire, spoken by hand: shape frozen, malformed frames refused.
// ---------------------------------------------------------------------------

/// Opens a data-plane stream to `target`, writes `frame`, returns the reply.
async fn raw_exchange(raw: &RawPeer, target: &TestCore, frame: Value) -> Value {
    let peer = onedevice_core::PeerAddr {
        node_id: target.key().node_id(),
        relay_url: Some(format!("iroh+memory://{}", target.key().node_id())),
        addrs: Vec::new(),
    };
    let mut stream = raw
        .transport
        .open(&peer)
        .await
        .expect("the switchboard routes");
    let bytes = serde_json::to_vec(&frame).unwrap();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .expect("frame length");
    stream.write_all(&bytes).await.expect("frame body");
    stream.flush().await.expect("flush");
    let mut len = [0u8; 4];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_exact(&mut len),
    )
    .await
    .expect("an ack in time")
    .expect("an ack");
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut body).await.expect("ack body");
    serde_json::from_slice(&body).expect("ack json")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wire_ack_reports_delivery_and_refuses_malformed_frames() {
    let server = TestServer::start().await;
    let switchboard = onedevice_test_support::memory_transport::MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let b = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let mut mb = messenger(&b, "engine", "custom").await;
    wait_server_connected(&mut mb, true).await;
    let raw = RawPeer::attested(&server, &switchboard, &code).await;
    wait_attested(&mut mb, &raw.device_id).await;

    // Well-formed: delivered, and the receiver hears it under the RAW PEER's
    // directory name, not any claim in the frame.
    let ack = raw_exchange(
        &raw,
        &b,
        json!({ "type": "peer_msg", "role": "custom", "payload": { "hi": true } }),
    )
    .await;
    assert_eq!(ack["type"], json!("peer_ack"));
    assert_eq!(ack["delivered"], json!(true));
    let msg = mb.wait_notification("peer.message").await;
    assert_eq!(msg["device_id"], json!(raw.device_id));

    // A role nobody holds here: same ack, delivered false.
    let ack = raw_exchange(
        &raw,
        &b,
        json!({ "type": "peer_msg", "role": "menu-backend", "payload": {} }),
    )
    .await;
    assert_eq!(ack["delivered"], json!(false));

    // A payload-less frame is malformed: refused, nothing delivered.
    let ack = raw_exchange(&raw, &b, json!({ "type": "peer_msg", "role": "custom" })).await;
    assert_eq!(ack["delivered"], json!(false));
    mb.assert_silent().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_wire_payload_is_refused_on_arrival() {
    let server = TestServer::start().await;
    let switchboard = onedevice_test_support::memory_transport::MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let b = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let mut mb = messenger(&b, "engine", "custom").await;
    wait_server_connected(&mut mb, true).await;
    let raw = RawPeer::attested(&server, &switchboard, &code).await;
    wait_attested(&mut mb, &raw.device_id).await;

    // A compliant sender's Core caps at 64 KiB; a hostile one can push up to
    // the frame limit. The receiver's own cap refuses it: nothing delivered.
    let ack = raw_exchange(
        &raw,
        &b,
        json!({ "type": "peer_msg", "role": "custom", "payload": "x".repeat(70 * 1024) }),
    )
    .await;
    assert_eq!(ack["delivered"], json!(false));
    mb.assert_silent().await;
}
