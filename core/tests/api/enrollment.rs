// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Enrollment of third-party components: pending request, approval with
//! reduced scopes, denial, persistent token (doc/core-api.md and
//! doc/architecture.md, "Security and enrollment").

use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn tokenless_hello_is_pending_and_methods_blocked() {
    let core = TestCore::start().await;
    let mut c = pending_component(&core, "third-party", "custom", &["devices.read"]).await;

    let err = c.request("session.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "PENDING_APPROVAL");
}

#[tokio::test]
async fn gui_receives_component_pending() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;
    let _c = pending_component(
        &core,
        "third-party",
        "custom",
        &["devices.read", "files.send"],
    )
    .await;

    // Pushed without a subscription: it is the duty of the holder of the
    // approve scope.
    let p = g.expect_notification("component.pending").await;
    assert!(
        p["request_id"]
            .as_str()
            .expect("request_id")
            .starts_with("r_"),
        "request_id prefixed with r_: {p}"
    );
    assert_eq!(p["name"], "third-party");
    assert_eq!(p["role"], "custom");
    assert_eq!(p["scopes"], json!(["devices.read", "files.send"]));
    assert!(p["peer_info"].is_object(), "peer_info object: {p}");
    if cfg!(any(target_os = "linux", windows)) {
        assert!(
            p["peer_info"]["pid"].is_u64(),
            "peer pid expected on this platform: {p}"
        );
        // "binary, pid — from the peer credentials" (spec): the binary is the
        // only datum of the prompt that is not self-declared.
        assert!(
            p["peer_info"]["exe"].is_string(),
            "peer binary path expected on this platform: {p}"
        );
    }
}

#[tokio::test]
async fn pending_requests_are_listed_for_late_gui() {
    let core = TestCore::start().await;
    let _c = pending_component(&core, "third-party", "custom", &["devices.read"]).await;

    // The GUI arrives after the request: the snapshot hands it over.
    let mut g = gui(&core).await;
    let list = g
        .request("components.pending", json!({}))
        .await
        .expect("components.pending");
    let reqs = list.as_array().expect("list of requests");
    assert_eq!(reqs.len(), 1, "one request expected: {list}");
    assert_eq!(reqs[0]["name"], "third-party");
    assert_eq!(reqs[0]["role"], "custom");
    assert_eq!(reqs[0]["scopes"], json!(["devices.read"]));
    assert!(
        reqs[0]["request_id"]
            .as_str()
            .expect("request_id")
            .starts_with("r_")
    );
}

#[tokio::test]
async fn approve_activates_with_granted_subset() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;
    let mut c = pending_component(
        &core,
        "third-party",
        "custom",
        &["devices.read", "files.send"],
    )
    .await;
    let p = g.expect_notification("component.pending").await;
    let request_id = p["request_id"].as_str().expect("request_id");

    g.request(
        "components.approve",
        json!({ "request_id": request_id, "scopes": ["devices.read"] }),
    )
    .await
    .expect("components.approve");

    let d = c.expect_notification("enrollment.decided").await;
    assert_eq!(d["approved"], true);
    assert_eq!(d["granted_scopes"], json!(["devices.read"]));
    assert!(d["token"].is_string(), "persistent token expected: {d}");

    // The connection is active, bounded to the granted scopes.
    c.request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .expect("subscribe on a granted scope");
    let err = c
        .request("events.subscribe", json!({ "topics": ["transfers"] }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn approve_cannot_exceed_requested_scopes() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;
    let mut c = pending_component(&core, "third-party", "custom", &["devices.read"]).await;
    let p = g.expect_notification("component.pending").await;
    let request_id = p["request_id"].as_str().expect("request_id");

    let err = g
        .request(
            "components.approve",
            json!({ "request_id": request_id, "scopes": ["devices.read", "files.send"] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);

    // The request is not consumed by the invalid attempt.
    let list = g
        .request("components.pending", json!({}))
        .await
        .expect("components.pending");
    assert_eq!(list.as_array().expect("list").len(), 1);
    c.assert_silent().await;
}

#[tokio::test]
async fn approve_never_grants_components_approve() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;
    let _c = pending_component(
        &core,
        "ambitious-third-party",
        "custom",
        &["devices.read", "components.approve"],
    )
    .await;
    let p = g.expect_notification("component.pending").await;
    let request_id = p["request_id"].as_str().expect("request_id");

    // Anti-escalation safeguard: even when requested, this scope is not
    // grantable through the prompt — only through the bootstrap trust roots.
    let err = g
        .request(
            "components.approve",
            json!({ "request_id": request_id, "scopes": ["components.approve"] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);

    // The rest of the request remains approvable.
    g.request(
        "components.approve",
        json!({ "request_id": request_id, "scopes": ["devices.read"] }),
    )
    .await
    .expect("approve of the legitimate subset");
}

#[tokio::test]
async fn deny_notifies_then_closes() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;
    let mut c = pending_component(&core, "third-party", "custom", &["devices.read"]).await;
    let p = g.expect_notification("component.pending").await;
    let request_id = p["request_id"].as_str().expect("request_id");

    g.request("components.deny", json!({ "request_id": request_id }))
        .await
        .expect("components.deny");

    let d = c.expect_notification("enrollment.decided").await;
    assert_eq!(d["approved"], false);
    assert!(d.get("token").is_none(), "no token on a denial: {d}");
    c.expect_close().await;
}

#[tokio::test]
async fn approved_token_is_persistent() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;
    let c = pending_component(&core, "third-party", "custom", &["devices.read"]).await;
    let p = g.expect_notification("component.pending").await;
    let request_id = p["request_id"].as_str().expect("request_id");
    g.request(
        "components.approve",
        json!({ "request_id": request_id, "scopes": ["devices.read"] }),
    )
    .await
    .expect("approve");
    let mut c = c;
    let d = c.expect_notification("enrollment.decided").await;
    let token = d["token"].as_str().expect("token").to_string();
    drop(c);

    // Nominal reconnection: the token is enough, no new pass through the queue.
    let mut again = core.connect().await;
    let r = again
        .hello("third-party", "custom", &["devices.read"], Some(&token))
        .await
        .expect("hello with the granted token");
    assert_eq!(r["status"], "ok");
    assert_eq!(r["granted_scopes"], json!(["devices.read"]));
}

#[tokio::test]
async fn enrolled_token_scopes_are_bounded() {
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
    drop(c);

    let mut again = core.connect().await;
    let err = again
        .hello(
            "third-party",
            "custom",
            &["devices.read", "files.send"],
            Some(&token),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");

    let err = again
        .hello(
            "third-party",
            "menu-backend",
            &["devices.read"],
            Some(&token),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "INVALID_TOKEN");
}

#[tokio::test]
async fn unknown_request_id_is_invalid_params() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;

    let err = g
        .request(
            "components.approve",
            json!({ "request_id": "r_nonexistent", "scopes": [] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);

    let err = g
        .request("components.deny", json!({ "request_id": "r_nonexistent" }))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn approve_of_conflicting_role_keeps_request_pending() {
    let core = TestCore::start().await;
    let _official = spawn_component(
        &core,
        "clip-official",
        "clipboard-backend",
        &["clipboard.read", "clipboard.write"],
    )
    .await;
    let mut g = gui(&core).await;
    let mut c = pending_component(
        &core,
        "clip-third-party",
        "clipboard-backend",
        &["clipboard.read"],
    )
    .await;
    let p = g.expect_notification("component.pending").await;
    let request_id = p["request_id"].as_str().expect("request_id");

    let err = g
        .request(
            "components.approve",
            json!({ "request_id": request_id, "scopes": ["clipboard.read"] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "ROLE_CONFLICT");

    // The request survives the conflict: approvable later, once the backend is
    // removed.
    let list = g
        .request("components.pending", json!({}))
        .await
        .expect("components.pending");
    assert_eq!(list.as_array().expect("list").len(), 1);
    c.assert_silent().await;
}

#[tokio::test]
async fn pending_disconnect_withdraws_request() {
    let core = TestCore::start().await;
    let mut g = gui(&core).await;
    let c = pending_component(&core, "third-party", "custom", &["devices.read"]).await;
    let p = g.expect_notification("component.pending").await;
    let request_id = p["request_id"].as_str().expect("request_id").to_string();

    drop(c);

    eventually(
        async || {
            let list = g
                .request("components.pending", json!({}))
                .await
                .expect("components.pending");
            list.as_array().expect("list").is_empty()
        },
        "withdrawal of the request after the requester disconnects",
    )
    .await;

    let err = g
        .request(
            "components.approve",
            json!({ "request_id": request_id, "scopes": ["devices.read"] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}
