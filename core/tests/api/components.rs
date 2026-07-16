// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Component management: inventory, revocation, protection by the
//! `components.approve` scope (doc/core-api.md, "components.*").

use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn list_shows_active_connections() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;
    let _tray = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let list = g
        .request("components.list", json!({}))
        .await
        .expect("components.list");

    let tray = find_component(&list, "tray-official");
    assert!(
        tray["component_id"]
            .as_str()
            .expect("component_id")
            .starts_with("c_")
    );
    assert_eq!(tray["role"], "tray");
    assert_eq!(tray["scopes"], json!(["session.read"]));
    assert_eq!(tray["connected"], true);
    // Bootstrap: no persistent token to revoke.
    assert_eq!(tray["enrolled"], false);

    // The calling GUI also appears in the inventory.
    let me = find_component(&list, "gui-test");
    assert_eq!(me["role"], "gui");
    assert_eq!(me["connected"], true);
    assert_eq!(me["enrolled"], false);
}

#[tokio::test]
async fn list_keeps_enrolled_components_when_disconnected() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;
    let mut c = pending_component(&core, "third-party", "custom", &["devices.read"]).await;
    let p = g.expect_notification("component.pending").await;
    let request_id = p["request_id"].as_str().expect("request_id");
    g.request(
        "components.approve",
        json!({ "request_id": request_id, "scopes": ["devices.read"] }),
    )
    .await
    .expect("approve");
    c.expect_notification("enrollment.decided").await;

    drop(c);

    // The enrollment (the token) outlives the connection: the inventory must
    // show it disconnected, not forget it.
    eventually(
        async || {
            let list = g
                .request("components.list", json!({}))
                .await
                .expect("components.list");
            list.as_array().expect("list").iter().any(|e| {
                e["name"] == "third-party" && e["connected"] == false && e["enrolled"] == true
            })
        },
        "enrolled component listed as disconnected",
    )
    .await;
}

#[tokio::test]
async fn revoke_closes_connection_and_invalidates_token() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;
    let mut c = pending_component(&core, "third-party", "custom", &["devices.read"]).await;
    let p = g.expect_notification("component.pending").await;
    let request_id = p["request_id"].as_str().expect("request_id");
    g.request(
        "components.approve",
        json!({ "request_id": request_id, "scopes": ["devices.read"] }),
    )
    .await
    .expect("approve");
    let d = c.expect_notification("enrollment.decided").await;
    let token = d["token"].as_str().expect("token").to_string();

    let list = g
        .request("components.list", json!({}))
        .await
        .expect("components.list");
    let component_id = find_component(&list, "third-party")["component_id"]
        .as_str()
        .expect("component_id")
        .to_string();

    g.request("components.revoke", json!({ "component_id": component_id }))
        .await
        .expect("components.revoke");

    // The connection drops, and the token reopens nothing.
    c.expect_close().await;
    let mut again = core.connect().await;
    let err = again
        .hello("third-party", "custom", &["devices.read"], Some(&token))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "INVALID_TOKEN");
}

#[tokio::test]
async fn revoke_unknown_component_is_invalid_params() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;

    let err = g
        .request(
            "components.revoke",
            json!({ "component_id": "c_nonexistent" }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn components_methods_require_approve_scope() {
    let core = TestCore::start().await;
    let mut tray = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    for method in ["components.list", "components.pending"] {
        let err = tray.request(method, json!({})).await.unwrap_err();
        assert_eq!(err.app_code(), "SCOPE_DENIED", "{method}");
    }
    let err = tray
        .request("components.revoke", json!({ "component_id": "c_x" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}
