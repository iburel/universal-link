// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The directory-resolution bridge: against a Core that HAS joined an
//! account, the engine resolves its own node_id from `devices.list` and
//! keeps serving the facade; a foreign-dialect peer payload is ignored
//! without ceremony. (The full multi-device exchange is unit-tested at the
//! engine layer; the facade gestures that would drive it over IPC arrive
//! with their own brick.)

use std::time::Duration;

use serde_json::json;

use crate::support::{Engine, RESPONSE_TIMEOUT, TestCore, status_eventually, ui};

/// A serverless Core that joined an account: device key + account root
/// seeded on disk, exactly what `account.setup` would have written.
async fn core_in_account() -> TestCore {
    TestCore::start_with(|dir| {
        let key = onedevice_test_support::DeviceKey::generate();
        std::fs::write(dir.join("device.key"), key.seed_hex()).expect("seed the device key");
        let code = onedevice_core::account_key::generate_recovery_code();
        let ak = onedevice_core::account_key::account_key_from_code(&code).expect("account key");
        let root = onedevice_core::account_key::root_for(&ak, &key.node_id());
        onedevice_core::account_key::save(dir, &root).expect("save the account root");
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_resolves_itself_and_keeps_serving() {
    let core = core_in_account().await;
    let engine = Engine::start(&core);
    let ui = ui(&core).await;

    // The facade serves while the engine resolves the directory behind it.
    let status = status_eventually(&ui).await;
    assert_eq!(status, json!({ "sets": [], "invitations": [] }));

    // Give the resolution a few ticks (200 ms in the tests), then prove the
    // loop is still healthy end to end: the facade still answers.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let status = tokio::time::timeout(
        RESPONSE_TIMEOUT,
        ui.client.request("sync.status", json!({})),
    )
    .await
    .expect("timely")
    .expect("served");
    assert_eq!(status, json!({ "sets": [], "invitations": [] }));

    assert_eq!(
        engine.stop().await,
        onedevice_sync::Outcome::StdinClosed,
        "the loop survived directory resolution and the safety ticks"
    );
}
