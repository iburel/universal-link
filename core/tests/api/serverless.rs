// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! An account with no server: being IN the account and being LOGGED IN are two
//! different things, and only the first is what the trust root, the directory and
//! the data plane rest on (C7). This file pins that separation.
//!
//! What it does NOT show yet: two serverless devices finding each other. A Core
//! here knows the account and knows itself, which is the foundation the signed
//! directory records and the LAN pairing build on — the following building
//! blocks. Until then a serverless Core has exactly one device in its directory,
//! and every peer check stays fail-closed.

use serde_json::json;

use crate::support::*;

/// A GUI-shaped component: manages the account and reads the directory.
async fn manager(core: &TestCore) -> TestComponent {
    spawn_component(
        core,
        "gui",
        "gui",
        &["session.manage", "session.read", "devices.read"],
    )
    .await
}

/// The only record a serverless Core has: itself. Returned so each test can
/// assert on it rather than re-derive the list shape.
async fn only_device(c: &mut TestComponent) -> serde_json::Value {
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let devices = list.as_array().expect("a list").clone();
    assert_eq!(devices.len(), 1, "one device, itself: {devices:?}");
    devices[0].clone()
}

/// No server URL, no OIDC, no login — and still an account. This is the gesture
/// the whole serverless story starts from: `account.setup` used to demand a
/// connected server, which made "create an account without one" impossible.
#[tokio::test(flavor = "multi_thread")]
async fn an_account_is_created_with_no_server_at_all() {
    let core = TestCore::start().await;
    let mut c = manager(&core).await;

    // Nothing configured: no session is even conceivable.
    let session = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(session["configured"], json!(false));
    assert_eq!(session["logged_in"], json!(false));

    let setup = c
        .request("account.setup", json!({}))
        .await
        .expect("account.setup with no server");
    assert!(
        setup["recovery_code"]
            .as_str()
            .is_some_and(|c| !c.is_empty()),
        "the code is the only copy of the account key: {setup}"
    );
    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true), "in the account");
    assert_eq!(status["holds_key"], json!(true), "and holding its key");
    assert_eq!(status["fingerprint"], setup["fingerprint"]);

    // And the directory answers, where it used to say SERVER_UNREACHABLE: a
    // device in the account knows at least itself.
    let own = only_device(&mut c).await;
    let node_id = core.node_id();
    assert_eq!(own["is_self"], json!(true));
    assert_eq!(own["node_id"], json!(node_id));
    assert_eq!(
        own["device_id"],
        json!(node_id),
        "no server named this device: its `device_id` is its own `node_id`"
    );
    assert_eq!(own["name"], json!(CORE_DEVICE_NAME));
    assert_eq!(own["platform"], json!(std::env::consts::OS));
    assert!(
        own["attestation"].as_str().is_some_and(|a| !a.is_empty()),
        "the record carries what proves it belongs: {own}"
    );
    // Its own liveness needs no server; a ROUTE to it still does (no relay
    // published, nobody seen on the LAN).
    assert_eq!(own["online"], json!(true));
    assert_eq!(own["lan"], json!(false));
    assert_eq!(own["reachable"], json!(false));
}

/// A serverless account is not a runtime accident: it comes back from disk, from
/// `account-key.json` alone — there is no `session.json` to hang it on.
#[tokio::test(flavor = "multi_thread")]
async fn the_account_survives_a_restart_with_no_server() {
    let code = universallink_core::account_key::generate_recovery_code();
    let core = TestCore::start_in_account(&code).await;
    let mut c = manager(&core).await;
    let before = only_device(&mut c).await;

    let core = core.restart().await;
    let mut c = manager(&core).await;

    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true), "still in the account");
    let after = only_device(&mut c).await;
    assert_eq!(after, before, "the same device, told the same way");
}

/// The directory cache expires because a server could refresh it. With no server
/// there is nothing to be stale with respect to, and expiring the file would not
/// fail closed — it would erase the account's other devices for good.
#[tokio::test(flavor = "multi_thread")]
async fn a_serverless_directory_does_not_expire() {
    let code = universallink_core::account_key::generate_recovery_code();
    let core = TestCore::start_in_account(&code).await;

    // A sibling of the same account — attested under the same key, as its own
    // device would be — written into a store far older than the cache TTL.
    let ak = universallink_core::account_key::account_key_from_code(&code).expect("valid code");
    let sibling = "b".repeat(64);
    let attestation = universallink_core::account_key::attest(&ak, &sibling);
    let ancient = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        - 90 * 24 * 60 * 60;
    let store = json!({
        "saved_at": ancient,
        "devices": {
            "peer-1": {
                "device_id": "peer-1",
                "name": "Old-Laptop",
                "platform": "linux",
                "node_id": sibling,
                "relay_url": null,
                "attestation": attestation,
                "online": false,
                "status": null,
                "last_seen": null,
            }
        }
    });
    std::fs::write(core.config_dir().join("directory.json"), store.to_string())
        .expect("write the store");

    let core = core.restart().await;
    let mut c = manager(&core).await;

    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let devices = list.as_array().expect("a list");
    assert_eq!(
        devices.len(),
        2,
        "the sibling from the store, plus ourselves: {devices:?}"
    );
    let peer = find_device(&list, "peer-1");
    assert_eq!(peer["name"], json!("Old-Laptop"));
    assert_eq!(peer["is_self"], json!(false));
}

/// The gate that was loosened must still hold where it was written for: a
/// deployment. Joining an account publishes an attestation the other devices read
/// from the server, and a server that is configured but unreachable cannot carry
/// it — so the device would be in the account for itself alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_server_that_is_down_still_refuses_to_create_an_account() {
    let server = TestServer::start().await;
    // Configured, never logged in: there is a server, and no connection to it.
    let core = TestCore::start_with_server(&server).await;
    let mut c = manager(&core).await;

    let refused = c
        .request("account.setup", json!({}))
        .await
        .expect_err("no server connection, no account");
    assert_eq!(refused.app_code(), "SERVER_UNREACHABLE");
    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(false));
}

/// The continuum, the easy half: an account born with no server can be pointed at
/// one afterwards. The device enrolls, the server names it — and it must not end
/// up appearing in its own directory twice, or as a stranger under the label it
/// minted for itself (`state::rekey_own`, whose exact effect is pinned by a unit
/// test — here it is the end state that matters).
#[tokio::test(flavor = "multi_thread")]
async fn a_serverless_account_can_be_pointed_at_a_server_afterwards() {
    let core = TestCore::start().await;
    let mut c = manager(&core).await;
    c.request("account.setup", json!({}))
        .await
        .expect("account.setup with no server");
    let fingerprint = c
        .request("account.status", json!({}))
        .await
        .expect("account.status")["fingerprint"]
        .clone();

    // The user points this device at a deployment — the GUI writes `config.json`,
    // the Core re-reads it — then logs in.
    let server = TestServer::start().await;
    core.stage_config(Some(server_cfg(&server)));
    let status = c
        .request("session.reload", json!({}))
        .await
        .expect("session.reload");
    assert_eq!(status["configured"], json!(true));
    complete_login(&mut c).await;
    wait_server_connected(&mut c, true).await;

    let own = only_device(&mut c).await;
    assert_eq!(own["is_self"], json!(true), "itself, not a stranger");
    assert_ne!(
        own["device_id"],
        json!(core.node_id()),
        "the self-minted label gave way to the server's"
    );
    assert_eq!(
        own["node_id"],
        json!(core.node_id()),
        "same crypto identity"
    );
    // The account did not move: same key, same fingerprint, and the attestation
    // the server now carries is the one derived from the code that never left
    // this machine.
    let after = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(after["fingerprint"], fingerprint);
    assert!(own["attestation"].as_str().is_some_and(|a| !a.is_empty()));
}

/// Logging out ends a session, not a membership: the trust root stays (a
/// re-enrollment or a pairing rests on it), so what this device knows first-hand
/// about itself stays too — under the self-minted `device_id`, the server's label
/// having left with the session.
#[tokio::test(flavor = "multi_thread")]
async fn logging_out_leaves_the_account_knowing_itself() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = manager(&core).await;
    complete_login(&mut c).await;
    c.request("account.setup", json!({}))
        .await
        .expect("account.setup");

    c.request("session.logout", json!({}))
        .await
        .expect("session.logout");

    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true), "still in the account");
    let own = only_device(&mut c).await;
    assert_eq!(own["is_self"], json!(true), "and still recognizes itself");
    assert_eq!(own["device_id"], json!(core.node_id()));
    assert_eq!(own["node_id"], json!(core.node_id()));
}
