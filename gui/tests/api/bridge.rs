// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The bridge: connection state (snapshot + events) and relaying of
//! notifications through to the webview.

use serde_json::json;

use crate::support::*;

#[tokio::test(flavor = "multi_thread")]
async fn connection_state_follows_the_core() {
    let mut core = TestCore::start().await;
    core.stop();
    let mut shell = shell_app(gui_config(&core)).await;

    // Core absent: fail-closed. The snapshot says "connecting", no initial
    // event is emitted, and requests fail immediately.
    assert_eq!(shell.connection_status().await["status"], "connecting");
    shell.assert_no_event().await;
    let err = shell
        .core_request("session.status", json!({}))
        .await
        .unwrap_err();
    assert_eq!(err["kind"], "not_connected");

    // The Core starts: the event carries the full snapshot, and the snapshot
    // queried afterwards is consistent.
    core.restart().await;
    let snap = shell.expect_connection("connected").await;
    assert_eq!(snap["api_version"], 1);
    assert_eq!(snap["granted_scopes"], json!(universallink_gui::GUI_SCOPES));
    assert_eq!(shell.connection_status().await["status"], "connected");

    // Loss of the Core: back to "connecting" — never a lying state.
    core.stop();
    shell.expect_connection("connecting").await;
    assert_eq!(shell.connection_status().await["status"], "connecting");

    // And reconnection is automatic: the bridge comes back to life without intervention.
    core.restart().await;
    shell.expect_connection("connected").await;
    let r = shell
        .core_request("session.status", json!({}))
        .await
        .expect("session.status after reconnection");
    assert_eq!(r["logged_in"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn notifications_reach_the_webview_and_approval_flows_back() {
    let core = TestCore::start().await;
    let mut shell = shell_app(gui_config(&core)).await;
    shell.wait_status("connected").await;

    // An unknown third-party component shows up: hello → pending, the Core
    // notifies the GUI (scope components.approve, without a subscription).
    let mut third = core.connect_raw().await;
    let r = third
        .hello("hiker", "custom", &["devices.read"], None)
        .await;
    assert_eq!(r["status"], "pending");

    let params = shell.wait_core_notification("component.pending").await;
    assert_eq!(params["name"], "hiker");
    assert_eq!(params["role"], "custom");
    let request_id = params["request_id"].clone();
    assert!(request_id.is_string(), "{params}");

    // The user approves from the webview; the decision reaches the third-party
    // component — the full loop of the approval prompt.
    shell
        .core_request(
            "components.approve",
            json!({ "request_id": request_id, "scopes": ["devices.read"] }),
        )
        .await
        .expect("components.approve");
    let decided = third.expect_notification("enrollment.decided").await;
    assert_eq!(decided["approved"], true);
    assert!(decided["token"].is_string(), "{decided}");
}

#[tokio::test(flavor = "multi_thread")]
async fn notifications_are_relayed_in_order() {
    let core = TestCore::start().await;
    let mut shell = shell_app(gui_config(&core)).await;
    shell.wait_status("connected").await;

    // Two enrollment requests sequenced: the response to the first's hello
    // precedes the second's request, so the order on the Core side is certain.
    let mut first = core.connect_raw().await;
    let r = first
        .hello("first", "custom", &["devices.read"], None)
        .await;
    assert_eq!(r["status"], "pending");
    let mut second = core.connect_raw().await;
    let r = second
        .hello("second", "custom", &["devices.read"], None)
        .await;
    assert_eq!(r["status"], "pending");

    // The first one arrives (the residual initial "connected" is tolerated),
    // then the STRICTLY next event must be the second: relayed in order,
    // without any spurious event interleaved.
    let p1 = shell.wait_core_notification("component.pending").await;
    assert_eq!(p1["name"], "first");
    let (name, payload) = shell.next_event().await;
    assert_eq!(name, "core:notification", "{payload}");
    assert_eq!(payload["method"], "component.pending", "{payload}");
    assert_eq!(payload["params"]["name"], "second", "{payload}");
}
