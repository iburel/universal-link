// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! A device struck off the account, and the one property that makes a tombstone
//! worth having: it is the ACCOUNT that struck it off, so no server puts it back.
//! The tombstone is a signature under the account key (C7) — the key the server
//! does not hold — while a directory snapshot is signed by nothing at all.
//!
//! Which is not a hypothetical: a deployment that was never told about the
//! revocation goes on listing the device, at every reconnection and in every
//! `device.*` it broadcasts. So the check sits at each door into the directory
//! (the store at startup, the snapshot, the events) rather than on the
//! authorization path alone — a struck-off device never enters the map that
//! `devices.list` serves and that every peer check reads.
//!
//! Minting a tombstone with no server in sight is `serverless.rs`; what is under
//! test here is a Core that holds one AND a server that disagrees.

use serde_json::{Value, json};
use universallink_test_support::memory_transport::MemorySwitchboard;

use crate::support::*;

/// Is `device_id` in a `devices.list` reply? Unlike `find_device`, absence is an
/// answer here rather than a panic — it is what half of this file asserts.
fn lists(list: &Value, device_id: &str) -> bool {
    list.as_array()
        .expect("a list")
        .iter()
        .any(|device| device["device_id"] == device_id)
}

/// A GUI-shaped component: reads the directory and sends files.
async fn gui(core: &TestCore) -> TestComponent {
    spawn_component(
        core,
        "gui",
        "gui",
        &["session.read", "devices.read", "files.send"],
    )
    .await
}

/// The whole point, end to end: the account struck a device off, the server never
/// heard about it, and the device stays out — of the snapshot the server hands us
/// at every reconnection, of the events it broadcasts afterwards, and of the
/// routes.
#[tokio::test(flavor = "multi_thread")]
async fn a_struck_off_device_stays_out_whatever_the_server_says() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let core = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let (struck, struck_id, mut struck_conn) = attested_sibling(&server, &code, "Lost-Phone").await;
    let mut c = gui(&core).await;
    wait_server_connected(&mut c, true).await;
    // Legitimate as far as the server is concerned, and as far as C7 is
    // concerned: what follows is about the revocation and nothing else.
    wait_attested(&mut c, &struck_id).await;

    // The account strikes it off. Seeded and read back through a restart rather
    // than minted here: what matters to this file is a Core that HOLDS a tombstone
    // against a server that disagrees. Where one comes FROM — this Core's own
    // `devices.revoke`, or a sibling's roster — is `serverless.rs` and
    // `dirsync.rs`.
    seed_revocation(core.config_dir(), &code, &struck.node_id());
    let core = core.restart().await;
    let mut c = gui(&core).await;
    wait_server_connected(&mut c, true).await;

    // The snapshot that connection just took listed it. It is not in the
    // directory.
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let devices = list.as_array().expect("a list");
    assert_eq!(
        devices.len(),
        1,
        "ourselves alone, though the server still lists the struck-off one: {devices:?}"
    );
    assert_eq!(devices[0]["is_self"], json!(true));

    // No route to it either: nothing left to resolve, so nothing leaves.
    let (_d, src) = scratch_file(b"hi");
    let path = src.to_str().expect("path");
    let refused = c
        .request(
            "files.send",
            json!({ "device_id": struck_id, "paths": [path] }),
        )
        .await
        .expect_err("struck off → no route");
    assert_eq!(refused.app_code(), "DEVICE_UNKNOWN");

    // And it does not come back through a LIVE event. The struck-off device
    // republishes its presence: the server, which knows nothing of the
    // revocation, broadcasts a `device.updated` carrying its record — attestation
    // included, and perfectly valid.
    struck_conn
        .request(
            "presence.update",
            json!({ "relay_url": format!("iroh+memory://{}/again", struck.node_id()) }),
        )
        .await
        .expect("the struck-off device speaks up");
    // Another device of the account comes online AFTER that, and its arrival is
    // the marker that the update was processed: server events travel in order, on
    // one connection.
    let present = server.online_device("Present-Laptop", "linux").await;
    eventually(
        async || {
            let list = c
                .request("devices.list", json!({}))
                .await
                .expect("devices.list");
            lists(&list, &present.device_id)
        },
        "a device the account did NOT strike off still arrives",
    )
    .await;
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert!(
        !lists(&list, &struck_id),
        "still out, after a full round of server events: {list}"
    );
}
