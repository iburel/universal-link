// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! `auth.challenge` → `auth.enroll`: enrolling a device
//! (doc/server-api.md, "Enrollment" and "Sensitive operations").

use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::sleep;

use crate::support::*;

/// Raw `auth.enroll`, without panicking on error (the harness helpers only
/// cover the nominal path and mask the full result).
async fn try_enroll(
    conn: &mut TestConn,
    id_token: String,
    node_id: String,
    proof: String,
) -> Result<Value, RpcError> {
    conn.request(
        "auth.enroll",
        json!({
            "id_token": id_token,
            "node_id": node_id,
            "name": "TestDevice",
            "platform": "linux",
            "proof": proof,
        }),
    )
    .await
}

#[tokio::test]
async fn enroll_happy_path() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    let result = conn
        .request(
            "auth.enroll",
            json!({
                "id_token": env.oidc.id_token("alice"),
                "node_id": key.node_id(),
                "name": "Desktop-PC",
                "platform": "linux",
                "proof": key.proof(&nonce),
            }),
        )
        .await
        .expect("auth.enroll");

    let device_id = result["device_id"].as_str().expect("device_id");
    assert!(device_id.starts_with("d_"), "device_id: {device_id}");
    assert_eq!(result["api_version"], 1);

    let device = &result["device"];
    assert_eq!(device["device_id"], device_id);
    assert_eq!(device["name"], "Desktop-PC");
    assert_eq!(device["platform"], "linux");
    assert_eq!(device["node_id"], key.node_id());
    // Enrolled but not authenticated: not online yet.
    assert_eq!(device["online"], false);
    assert_eq!(device["status"], Value::Null);
}

#[tokio::test]
async fn enroll_notifies_other_devices() {
    let env = TestEnv::start().await;
    let mut observer = online_device(&env, "alice", "Observer", "linux").await;
    observer.conn.drain().await;

    let newcomer = enroll_device(&env, "alice", "Newcomer", "macos").await;

    let params = observer.conn.expect_notification("device.added").await;
    assert_eq!(params["device"]["device_id"], newcomer.device_id.as_str());
    assert_eq!(params["device"]["online"], false);
}

#[tokio::test]
async fn enroll_not_broadcast_to_other_accounts() {
    let env = TestEnv::start().await;
    let mut observer = online_device(&env, "alice", "Observer", "linux").await;
    observer.conn.drain().await;

    let _other = enroll_device(&env, "bob", "Stranger", "windows").await;

    observer.conn.assert_silent().await;
}

#[tokio::test]
async fn enroll_rejects_wrong_signature() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    let token = env.oidc.id_token_wrong_key("alice");
    let err = try_enroll(&mut conn, token, key.node_id(), key.proof(&nonce))
        .await
        .expect_err("invalid OIDC signature");
    assert_eq!(err.app_code(), "OIDC_INVALID");
}

#[tokio::test]
async fn enroll_rejects_wrong_audience() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    let token = env.oidc.id_token_with("alice", |c| {
        c.insert("aud".into(), json!("other-client"));
    });
    let err = try_enroll(&mut conn, token, key.node_id(), key.proof(&nonce))
        .await
        .expect_err("unexpected aud");
    assert_eq!(err.app_code(), "OIDC_INVALID");
}

#[tokio::test]
async fn enroll_rejects_wrong_issuer() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    let token = env.oidc.id_token_with("alice", |c| {
        c.insert("iss".into(), json!("https://attacker.example"));
    });
    let err = try_enroll(&mut conn, token, key.node_id(), key.proof(&nonce))
        .await
        .expect_err("unexpected iss");
    assert_eq!(err.app_code(), "OIDC_INVALID");
}

#[tokio::test]
async fn enroll_rejects_expired_token() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    // `iat` stays within the freshness window: only `exp` is at fault.
    let now = unix_now();
    let token = env.oidc.id_token_with("alice", |c| {
        c.insert("iat".into(), json!(now - 120));
        c.insert("exp".into(), json!(now - 60));
    });
    let err = try_enroll(&mut conn, token, key.node_id(), key.proof(&nonce))
        .await
        .expect_err("expired token");
    assert_eq!(err.app_code(), "OIDC_INVALID");
}

#[tokio::test]
async fn enroll_rejects_stale_token() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    // Token not expired but with an `iat` older than `max_fresh_token_age`
    // (300 s by default): enroll is a sensitive operation, a fresh token is required.
    let token = env.oidc.id_token_with("alice", |c| {
        c.insert("iat".into(), json!(unix_now() - 3600));
    });
    let err = try_enroll(&mut conn, token, key.node_id(), key.proof(&nonce))
        .await
        .expect_err("token not fresh enough");
    assert_eq!(err.app_code(), "OIDC_INVALID");
}

// Absent `aud` and `iss` must be refused, not just wrong values: a token that
// omits the claim proves nothing about its intended recipient.

#[tokio::test]
async fn enroll_rejects_token_without_audience() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    let token = env.oidc.id_token_with("alice", |c| {
        c.remove("aud");
    });
    let err = try_enroll(&mut conn, token, key.node_id(), key.proof(&nonce))
        .await
        .expect_err("aud missing");
    assert_eq!(err.app_code(), "OIDC_INVALID");
}

#[tokio::test]
async fn enroll_rejects_token_without_issuer() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    let token = env.oidc.id_token_with("alice", |c| {
        c.remove("iss");
    });
    let err = try_enroll(&mut conn, token, key.node_id(), key.proof(&nonce))
        .await
        .expect_err("iss missing");
    assert_eq!(err.app_code(), "OIDC_INVALID");
}

// The issuer rotates its signing keys periodically. The server caches the JWKS,
// so it must re-fetch when a token arrives under a key id it has not seen —
// otherwise every enrollment fails from the first rotation until a restart.

#[tokio::test]
async fn enroll_accepts_token_after_idp_key_rotation() {
    // No refresh cooldown: the rotation and the retry happen within one test,
    // far under the production floor.
    let env = TestEnv::start_with(|c| c.oidc.jwks_refresh_min_interval = Duration::ZERO).await;

    // A first enroll makes the server fetch and cache the issuer's JWKS.
    let _ = enroll_device(&env, "alice", "Before", "linux").await;

    // The IdP rotates: new tokens carry a `kid` absent from the cached JWKS.
    env.oidc.rotate_signing_key();

    // The server must re-fetch the JWKS on the key-id miss and accept the token.
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;
    let result = try_enroll(
        &mut conn,
        env.oidc.id_token("alice"),
        key.node_id(),
        key.proof(&nonce),
    )
    .await
    .expect("enroll after rotation");
    assert!(
        result["device_id"]
            .as_str()
            .expect("device_id")
            .starts_with("d_")
    );
}

#[tokio::test]
async fn unknown_key_id_does_not_refetch_jwks_within_cooldown() {
    // With a real cooldown, a flood of tokens bearing unknown key ids must not
    // become one JWKS request per token (amplification against the issuer).
    let env =
        TestEnv::start_with(|c| c.oidc.jwks_refresh_min_interval = Duration::from_secs(60)).await;

    for _ in 0..3 {
        let mut conn = env.connect().await;
        let key = DeviceKey::generate();
        let nonce = challenge(&mut conn).await;
        let err = try_enroll(
            &mut conn,
            env.oidc.id_token_unknown_kid("alice"),
            key.node_id(),
            key.proof(&nonce),
        )
        .await
        .expect_err("unknown kid");
        assert_eq!(err.app_code(), "OIDC_INVALID");
    }

    // Exactly one fetch: the first miss re-fetched (the cache was empty), the
    // rest fell inside the cooldown.
    assert_eq!(env.oidc.jwks_fetch_count(), 1);
}

#[tokio::test]
async fn enroll_rejects_unknown_platform() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    // `platform` is a closed enumeration in the device record.
    let err = conn
        .request(
            "auth.enroll",
            json!({
                "id_token": env.oidc.id_token("alice"),
                "node_id": key.node_id(),
                "name": "PC",
                "platform": "amiga",
                "proof": key.proof(&nonce),
            }),
        )
        .await
        .expect_err("platform outside the enumeration");
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn enroll_rejects_oversized_name() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    // The record's fields are rebroadcast in every notification: they are bounded.
    let err = conn
        .request(
            "auth.enroll",
            json!({
                "id_token": env.oidc.id_token("alice"),
                "node_id": key.node_id(),
                "name": "x".repeat(10_000),
                "platform": "linux",
                "proof": key.proof(&nonce),
            }),
        )
        .await
        .expect_err("oversized name");
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn enroll_rejects_bad_proof() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let other_key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    // proof signed by a key other than node_id: the proof of possession fails.
    let err = try_enroll(
        &mut conn,
        env.oidc.id_token("alice"),
        key.node_id(),
        other_key.proof(&nonce),
    )
    .await
    .expect_err("proof from a different key");
    assert_eq!(err.app_code(), "INVALID_PROOF");
}

#[tokio::test]
async fn enroll_rejects_replayed_nonce() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let nonce = challenge(&mut conn).await;

    let first_key = DeviceKey::generate();
    try_enroll(
        &mut conn,
        env.oidc.id_token("alice"),
        first_key.node_id(),
        first_key.proof(&nonce),
    )
    .await
    .expect("first enroll");

    // The nonce is single-use: replaying it, even with a correctly signed
    // proof, must fail.
    let second_key = DeviceKey::generate();
    let err = try_enroll(
        &mut conn,
        env.oidc.id_token("alice"),
        second_key.node_id(),
        second_key.proof(&nonce),
    )
    .await
    .expect_err("replayed nonce");
    assert_eq!(err.app_code(), "INVALID_PROOF");
}

#[tokio::test]
async fn enroll_rejects_expired_nonce() {
    let env = TestEnv::start_with(|c| c.nonce_ttl = Duration::from_millis(200)).await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();
    let nonce = challenge(&mut conn).await;

    sleep(Duration::from_millis(500)).await;

    let err = try_enroll(
        &mut conn,
        env.oidc.id_token("alice"),
        key.node_id(),
        key.proof(&nonce),
    )
    .await
    .expect_err("expired nonce");
    assert_eq!(err.app_code(), "INVALID_PROOF");
}

#[tokio::test]
async fn enroll_requires_prior_challenge() {
    let env = TestEnv::start().await;
    let mut conn = env.connect().await;
    let key = DeviceKey::generate();

    // No auth.challenge on this connection: the proof signs a made-up nonce
    // that the server never issued.
    let err = try_enroll(
        &mut conn,
        env.oidc.id_token("alice"),
        key.node_id(),
        key.proof("invented-nonce"),
    )
    .await
    .expect_err("no prior challenge");
    assert_eq!(err.app_code(), "INVALID_PROOF");
}

#[tokio::test]
async fn enroll_does_not_authenticate_connection() {
    let env = TestEnv::start().await;
    let mut device = enroll_device(&env, "alice", "Desktop-PC", "linux").await;

    let err = device
        .conn
        .request("devices.list", json!({}))
        .await
        .expect_err("connection not authenticated after enroll");
    assert_eq!(err.app_code(), "NOT_AUTHENTICATED");
}
