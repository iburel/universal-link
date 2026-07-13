// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Directory persistence: what SURVIVES a server restart (device identity, C7
//! attestation, revocations). The restart is simulated by sharing a single
//! memory store between two successive servers: the first writes, we drop it,
//! the second reloads the same state.

use std::sync::Arc;

use serde_json::json;

use crate::support::*;
use universallink_server::MemoryStore;

#[tokio::test]
async fn enrolled_device_survives_a_restart() {
    let store = Arc::new(MemoryStore::default());
    let attestation = "ab".repeat(64);

    // Server 1: a device enrolls, authenticates, publishes its C7 attestation.
    let device = {
        let env = TestEnv::start_with_store(store.clone()).await;
        let mut device = online_device(&env, "alice", "pc-a", "linux").await;
        device
            .conn
            .request("presence.update", json!({ "attestation": attestation }))
            .await
            .expect("presence.update");
        device
        // `env` is dropped here: server 1 stops.
    };

    // Server 2 on the SAME store: the device re-authenticates (without OIDC, just
    // its key) and is found again in the directory, attestation included —
    // otherwise a peer would refuse it fail-closed.
    let env2 = TestEnv::start_with_store(store.clone()).await;
    let mut conn = reconnect(&env2, &device).await;
    let list = conn
        .request("devices.list", json!({}))
        .await
        .expect("devices.list after restart");
    let record = find_device(&list, &device.device_id);
    assert_eq!(record["online"], true);
    assert_eq!(record["name"], "pc-a");
    assert_eq!(record["node_id"], device.key.node_id());
    assert_eq!(
        record["attestation"], attestation,
        "attestation lost across restart → the peer would refuse the device"
    );
}

#[tokio::test]
async fn a_revoked_device_stays_revoked_after_restart() {
    let store = Arc::new(MemoryStore::default());

    // Server 1: two devices, we revoke one.
    let victim = {
        let env = TestEnv::start_with_store(store.clone()).await;
        let mut keeper = online_device(&env, "alice", "keeper", "linux").await;
        let victim = enroll_device(&env, "alice", "victim", "windows").await;
        keeper
            .conn
            .request(
                "devices.revoke",
                json!({ "device_id": victim.device_id, "id_token": env.oidc.id_token("alice") }),
            )
            .await
            .expect("devices.revoke");
        victim
    };

    // Server 2 on the SAME store: the victim must not be able to
    // re-authenticate — the revocation survived the restart.
    let env2 = TestEnv::start_with_store(store.clone()).await;
    let mut conn = env2.connect().await;
    let nonce = challenge(&mut conn).await;
    let err = conn
        .request(
            "auth.authenticate",
            json!({ "device_id": victim.device_id, "proof": victim.key.proof(&nonce) }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_REVOKED");
}
