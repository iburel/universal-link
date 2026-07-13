// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Event subscription: validation of topics and their scopes (doc/core-api.md,
//! "Event subscription"). The actual delivery of notifications per topic will
//! be covered with the server session.

use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn subscribe_with_matching_scopes() {
    let core = TestCore::start().await;
    let mut c = spawn_component(
        &core,
        "tray-official",
        "tray",
        &["session.read", "devices.read"],
    )
    .await;

    let r = c
        .request(
            "events.subscribe",
            json!({ "topics": ["session", "devices"] }),
        )
        .await
        .expect("events.subscribe");
    assert_eq!(r, json!({}));
}

#[tokio::test]
async fn subscribe_without_scope_is_denied() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let err = c
        .request(
            "events.subscribe",
            json!({ "topics": ["session", "transfers"] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn subscribe_unknown_topic_is_invalid_params() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let err = c
        .request("events.subscribe", json!({ "topics": ["weather"] }))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn subscribe_before_hello_reveals_nothing() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    // The phase comes before the params: even with invalid params, a
    // non-enrolled connection receives NOT_ENROLLED, not -32602 — it cannot
    // probe the shape of a method's parameters.
    let err = c
        .request("events.subscribe", json!({ "topics": 42 }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "NOT_ENROLLED");
}
