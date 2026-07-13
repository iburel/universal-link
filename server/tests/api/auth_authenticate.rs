// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Tests for `auth.authenticate` — nominal connection (doc/server-api.md).

use std::time::Duration;

use crate::support::*;

use serde_json::{Value, json};

/// `auth.challenge` + `auth.authenticate`, returning the raw result.
/// Bypasses the harness's `authenticate` helper, which masks `api_version`,
/// panics on error and does not take `relay_url`.
async fn try_authenticate(
    conn: &mut TestConn,
    key: &DeviceKey,
    device_id: &str,
    relay_url: Option<&str>,
) -> Result<Value, RpcError> {
    let nonce = challenge(conn).await;
    let mut params = json!({ "device_id": device_id, "proof": key.proof(&nonce) });
    if let Some(url) = relay_url {
        params["relay_url"] = json!(url);
    }
    conn.request("auth.authenticate", params).await
}

#[tokio::test]
async fn authenticate_happy_path() {
    let env = TestEnv::start().await;
    let mut device = enroll_device(&env, "alice", "Desktop-PC", "linux").await;

    let result = try_authenticate(&mut device.conn, &device.key, &device.device_id, None)
        .await
        .expect("auth.authenticate");

    assert_eq!(result["api_version"], json!(1));
    let record = &result["device"];
    assert_eq!(record["device_id"], device.device_id);
    assert_eq!(record["online"], true);

    let list = device
        .conn
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    find_device(&list, &device.device_id);
}

#[tokio::test]
async fn authenticate_notifies_others() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;
    let mut b = enroll_device(&env, "alice", "B", "macos").await;
    a.conn.drain().await;

    authenticate(&mut b.conn, &b.key, &b.device_id).await;

    let params = a.conn.wait_notification("device.online").await;
    assert_eq!(params["device"]["device_id"], b.device_id);
    assert_eq!(params["device"]["online"], true);
}

#[tokio::test]
async fn authenticate_unknown_device() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();

    let err = try_authenticate(&mut conn, &key, "d_0000000000000000", None)
        .await
        .expect_err("unknown device_id");
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");
}

#[tokio::test]
async fn authenticate_rejects_bad_proof() {
    let env = TestEnv::start().await;
    let mut device = enroll_device(&env, "alice", "A", "linux").await;
    let other_key = DeviceKey::generate();

    let err = try_authenticate(&mut device.conn, &other_key, &device.device_id, None)
        .await
        .expect_err("proof signed by a different key");
    assert_eq!(err.app_code(), "INVALID_PROOF");
}

#[tokio::test]
async fn authenticate_rejects_replayed_nonce() {
    let env = TestEnv::start().await;
    let mut device = enroll_device(&env, "alice", "A", "linux").await;

    let nonce = challenge(&mut device.conn).await;
    let params = json!({ "device_id": device.device_id, "proof": device.key.proof(&nonce) });

    device
        .conn
        .request("auth.authenticate", params.clone())
        .await
        .expect("first use of the nonce");

    // The nonce is single-use: replaying it counts as an invalid proof (spec, Errors).
    let err = device
        .conn
        .request("auth.authenticate", params)
        .await
        .expect_err("replayed nonce");
    assert_eq!(err.app_code(), "INVALID_PROOF");
}

#[tokio::test]
async fn authenticate_rejects_expired_nonce() {
    let env = TestEnv::start_with(|c| c.nonce_ttl = Duration::from_secs(2)).await;
    let mut device = enroll_device(&env, "alice", "A", "linux").await;

    let nonce = challenge(&mut device.conn).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let err = device
        .conn
        .request(
            "auth.authenticate",
            json!({ "device_id": device.device_id, "proof": device.key.proof(&nonce) }),
        )
        .await
        .expect_err("expired nonce");
    assert_eq!(err.app_code(), "INVALID_PROOF");
}

#[tokio::test]
async fn authenticate_requires_prior_challenge() {
    let env = TestEnv::start().await;
    let device = enroll_device(&env, "alice", "A", "linux").await;

    // New connection, no auth.challenge: the proof cannot match any nonce
    // issued for this connection.
    let mut conn = env.connect().await;
    let err = conn
        .request(
            "auth.authenticate",
            json!({
                "device_id": device.device_id,
                "proof": device.key.proof("nonce-never-issued"),
            }),
        )
        .await
        .expect_err("authenticate without a prior challenge");
    assert_eq!(err.app_code(), "INVALID_PROOF");
}

#[tokio::test]
async fn authenticate_revoked_device() {
    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;
    let b = online_device(&env, "alice", "B", "macos").await;

    a.conn
        .request(
            "devices.revoke",
            json!({
                "device_id": b.device_id,
                "id_token": env.oidc.id_token("alice"),
            }),
        )
        .await
        .expect("devices.revoke");

    let mut conn = env.connect().await;
    let err = try_authenticate(&mut conn, &b.key, &b.device_id, None)
        .await
        .expect_err("revoked device");
    assert_eq!(err.app_code(), "DEVICE_REVOKED");
}

#[tokio::test]
async fn methods_require_authentication() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;

    let calls = [
        ("devices.list", json!({})),
        ("devices.rename", json!({ "device_id": "d_x", "name": "X" })),
        (
            "devices.revoke",
            json!({ "device_id": "d_x", "id_token": env.oidc.id_token("alice") }),
        ),
        ("presence.update", json!({ "status": "idle" })),
    ];
    for (method, params) in calls {
        let err = conn.request(method, params).await.expect_err(method);
        assert_eq!(err.app_code(), "NOT_AUTHENTICATED", "method: {method}");
    }
}

#[tokio::test]
async fn authenticate_carries_relay_url() {
    const RELAY_URL: &str = "https://relay.test/eu-west";

    let env = TestEnv::start().await;
    let mut a = online_device(&env, "alice", "A", "linux").await;
    let mut b = enroll_device(&env, "alice", "B", "windows").await;
    a.conn.drain().await;

    try_authenticate(&mut b.conn, &b.key, &b.device_id, Some(RELAY_URL))
        .await
        .expect("auth.authenticate with relay_url");

    let params = a.conn.wait_notification("device.online").await;
    assert_eq!(params["device"]["device_id"], b.device_id);
    assert_eq!(params["device"]["relay_url"], RELAY_URL);

    let list = a
        .conn
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(find_device(&list, &b.device_id)["relay_url"], RELAY_URL);
}
