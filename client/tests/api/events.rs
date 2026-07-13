// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Relayed notifications and subscription done by the establishment cycle.

use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn component_pending_reaches_the_client() {
    let core = TestCore::start().await;
    // `component.pending` is pushed without a subscription to every holder of
    // the approval scope: notification relaying can be tested without a server.
    let (_client, mut events) = connected(&core, "gui", &["components.approve"], &[]).await;

    let mut third = core.connect_raw().await;
    let r = third
        .hello("clip-x", "custom", &["devices.read"], None)
        .await;
    assert_eq!(r["status"], "pending");

    let (method, params) = expect_notification(&mut events).await;
    assert_eq!(method, "component.pending");
    assert_eq!(params["name"], "clip-x");
    assert_eq!(params["role"], "custom");
    assert!(params["request_id"].as_str().is_some(), "{params}");
}

#[tokio::test]
async fn approve_through_the_client() {
    let core = TestCore::start().await;
    let (client, mut events) = connected(&core, "gui", &["components.approve"], &[]).await;

    let mut third = core.connect_raw().await;
    let r = third
        .hello("clip-x", "custom", &["devices.read"], None)
        .await;
    assert_eq!(r["status"], "pending");
    let (_, params) = expect_notification(&mut events).await;
    let request_id = params["request_id"].as_str().expect("request_id");

    client
        .request(
            "components.approve",
            json!({ "request_id": request_id, "scopes": ["devices.read"] }),
        )
        .await
        .expect("components.approve");

    let decided = third.expect_notification("enrollment.decided").await;
    assert_eq!(decided["approved"], true);
    assert!(decided["token"].as_str().is_some(), "{decided}");
}

#[tokio::test]
async fn topic_without_scope_never_connects() {
    let core = TestCore::start().await;
    // Subscribing to the devices topic without the scope: the cycle's
    // subscribe fails with SCOPE_DENIED — never Connected (faulty config,
    // fail-closed).
    let (_client, mut events) = universallink_ipc_client::spawn(client_config(
        &core,
        "gui",
        &["session.read"],
        &["devices"],
    ));
    assert_no_event(&mut events).await;
}
