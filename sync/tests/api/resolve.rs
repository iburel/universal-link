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

use crate::support::{Engine, RESPONSE_TIMEOUT, core_in_account, status_eventually, ui};

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
