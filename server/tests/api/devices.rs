// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Device directory: `devices.list`, `devices.rename`, `devices.revoke`
//! (doc/server-api.md — "The device record", "Methods", "Notifications").

use serde_json::{Value, json};

use crate::support::*;

// Local wrappers: the harness exposes no helpers for the `devices.*` methods,
// nor a fallible variant of the `authenticate` flow.

async fn list_devices(conn: &mut TestConn) -> Value {
    conn.request("devices.list", json!({}))
        .await
        .expect("devices.list")
}

async fn rename(conn: &mut TestConn, device_id: &str, name: &str) -> Result<Value, RpcError> {
    conn.request(
        "devices.rename",
        json!({ "device_id": device_id, "name": name }),
    )
    .await
}

async fn revoke(conn: &mut TestConn, device_id: &str, id_token: &str) -> Result<Value, RpcError> {
    conn.request(
        "devices.revoke",
        json!({ "device_id": device_id, "id_token": id_token }),
    )
    .await
}

#[tokio::test]
async fn list_contains_all_account_devices() {
    let env = TestEnv::start().await;
    let mut desktop = online_device(&env, "alice", "Desktop", "linux").await;
    let laptop = enroll_device(&env, "alice", "Laptop", "macos").await;

    let list = list_devices(&mut desktop.conn).await;
    assert_eq!(list.as_array().expect("list").len(), 2);

    let d = find_device(&list, &desktop.device_id);
    assert_eq!(d["online"], true);
    assert_eq!(d["name"], "Desktop");
    assert_eq!(d["platform"], "linux");
    assert_eq!(d["node_id"], desktop.key.node_id());

    // Enrolled but never authenticated: in the directory, offline
    // (`auth.enroll` does not bind the connection to the device).
    let d = find_device(&list, &laptop.device_id);
    assert_eq!(d["online"], false);
    assert_eq!(d["name"], "Laptop");
    assert_eq!(d["platform"], "macos");
    assert_eq!(d["node_id"], laptop.key.node_id());
}

#[tokio::test]
async fn list_scoped_to_account() {
    let env = TestEnv::start().await;
    let mut alice = online_device(&env, "alice", "Alice-PC", "linux").await;
    let bob = online_device(&env, "bob", "Bob-PC", "windows").await;

    let list = list_devices(&mut alice.conn).await;
    let devices = list.as_array().expect("list");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["device_id"], alice.device_id);
    assert!(devices.iter().all(|d| d["device_id"] != bob.device_id));
}

#[tokio::test]
async fn no_cross_account_notifications() {
    let env = TestEnv::start().await;
    let mut alice = online_device(&env, "alice", "Alice-PC", "linux").await;
    let mut bob1 = online_device(&env, "bob", "Bob-1", "windows").await;
    let bob2 = online_device(&env, "bob", "Bob-2", "macos").await;

    alice.conn.drain().await;

    // Changes that do generate notifications (bob2 is online), but only within
    // bob's account.
    rename(&mut bob1.conn, &bob2.device_id, "Bob-2-renamed")
        .await
        .expect("devices.rename");
    bob1.conn
        .request("presence.update", json!({ "status": "busy" }))
        .await
        .expect("presence.update");

    alice.conn.assert_silent().await;
}

#[tokio::test]
async fn rename_broadcasts_to_others() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;
    let mut b = online_device(&env, "alice", "B", "windows").await;
    let mut c = online_device(&env, "alice", "C", "macos").await;
    a.conn.drain().await;
    b.conn.drain().await;
    c.conn.drain().await;

    rename(&mut a.conn, &b.device_id, "B-renamed")
        .await
        .expect("devices.rename");

    let params = b.conn.expect_notification("device.updated").await;
    assert_eq!(params["device"]["device_id"], b.device_id);
    assert_eq!(params["device"]["name"], "B-renamed");

    let params = c.conn.expect_notification("device.updated").await;
    assert_eq!(params["device"]["device_id"], b.device_id);
    assert_eq!(params["device"]["name"], "B-renamed");

    // The requester has the response: no echo notification.
    a.conn.assert_silent().await;

    let list = list_devices(&mut a.conn).await;
    assert_eq!(find_device(&list, &b.device_id)["name"], "B-renamed");
}

#[tokio::test]
async fn rename_unknown_device() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;

    let err = rename(&mut a.conn, "d_unknown", "X").await.unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");
}

#[tokio::test]
async fn rename_rejects_oversized_name() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;

    let err = rename(&mut a.conn, &a.device_id, &"x".repeat(10_000))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

// Cross-account isolation holds for writes too: a valid device_id that belongs
// to another account is indistinguishable from an unknown id.

#[tokio::test]
async fn rename_cross_account_device_is_unknown() {
    let env = TestEnv::start().await;
    let mut alice = online_device(&env, "alice", "Alice-PC", "linux").await;
    let mut bob = online_device(&env, "bob", "Bob-PC", "windows").await;
    bob.conn.drain().await;

    let err = rename(&mut alice.conn, &bob.device_id, "pwned")
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");

    bob.conn.assert_silent().await;
}

#[tokio::test]
async fn revoke_cross_account_device_is_unknown() {
    let env = TestEnv::start().await;
    let mut alice = online_device(&env, "alice", "Alice-PC", "linux").await;
    let mut bob = online_device(&env, "bob", "Bob-PC", "windows").await;
    bob.conn.drain().await;

    let err = revoke(&mut alice.conn, &bob.device_id, &env.oidc.id_token("alice"))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");

    // Bob's device is neither closed nor notified.
    bob.conn.assert_silent().await;
    let list = list_devices(&mut bob.conn).await;
    assert_eq!(find_device(&list, &bob.device_id)["online"], true);
}

#[tokio::test]
async fn revoke_closes_and_broadcasts() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;
    let mut b = online_device(&env, "alice", "B", "windows").await;
    let mut c = online_device(&env, "alice", "C", "macos").await;
    b.conn.drain().await;
    c.conn.drain().await;

    revoke(&mut a.conn, &b.device_id, &env.oidc.id_token("alice"))
        .await
        .expect("devices.revoke");

    // The revoked device is not notified by message: a direct close.
    let (_, reason) = b
        .conn
        .expect_close_silent()
        .await
        .expect("close frame expected");
    assert_eq!(reason, "DEVICE_REVOKED");

    let params = c.conn.expect_notification("device.removed").await;
    assert_eq!(params["device_id"], b.device_id);

    let list = list_devices(&mut a.conn).await;
    assert!(
        list.as_array()
            .expect("liste")
            .iter()
            .all(|d| d["device_id"] != b.device_id),
        "revoked device still listed: {list}"
    );
}

#[tokio::test]
async fn revoked_device_cannot_reauthenticate() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;
    let b = online_device(&env, "alice", "B", "windows").await;

    revoke(&mut a.conn, &b.device_id, &env.oidc.id_token("alice"))
        .await
        .expect("devices.revoke");

    let mut conn = env.connect().await;
    let nonce = challenge(&mut conn).await;
    let err = conn
        .request(
            "auth.authenticate",
            json!({ "device_id": b.device_id, "proof": b.key.proof(&nonce) }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_REVOKED");
}

#[tokio::test]
async fn revoke_requires_fresh_id_token() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;
    let b = online_device(&env, "alice", "B", "windows").await;

    // Valid (unexpired) token but too old for a sensitive operation.
    let stale = env.oidc.id_token_with("alice", |claims| {
        claims.insert("iat".into(), json!(unix_now() - 3600));
    });
    let err = revoke(&mut a.conn, &b.device_id, &stale).await.unwrap_err();
    assert_eq!(err.app_code(), "OIDC_INVALID");

    let list = list_devices(&mut a.conn).await;
    let d = find_device(&list, &b.device_id);
    assert_eq!(d["online"], true);
}

#[tokio::test]
async fn revoke_rejects_invalid_id_token() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;
    let b = online_device(&env, "alice", "B", "windows").await;

    let token = env.oidc.id_token_wrong_key("alice");
    let err = revoke(&mut a.conn, &b.device_id, &token).await.unwrap_err();
    assert_eq!(err.app_code(), "OIDC_INVALID");
}

#[tokio::test]
async fn revoke_unknown_device() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;

    let err = revoke(&mut a.conn, "d_unknown", &env.oidc.id_token("alice"))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");
}

#[tokio::test]
async fn revoke_self() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;
    let mut b = online_device(&env, "alice", "B", "windows").await;
    a.conn.drain().await;
    b.conn.drain().await;

    // The requester receives its response BEFORE its connection is closed.
    revoke(&mut a.conn, &a.device_id, &env.oidc.id_token("alice"))
        .await
        .expect("devices.revoke on itself");

    let (_, reason) = a
        .conn
        .expect_close_silent()
        .await
        .expect("close frame expected");
    assert_eq!(reason, "DEVICE_REVOKED");

    let params = b.conn.expect_notification("device.removed").await;
    assert_eq!(params["device_id"], a.device_id);
}
