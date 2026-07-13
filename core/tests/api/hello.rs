// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! `hello` handshake: bootstrap tokens (spawn and file), scopes, roles and
//! exclusivity (doc/core-api.md, "Handshake and enrollment").

use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn spawn_token_grants_requested_scopes() {
    let core = TestCore::start().await;
    let token = core.mint("tray", &["session.read", "devices.read"]);
    let mut c = core.connect().await;

    let r = c
        .hello(
            "tray-official",
            "tray",
            &["session.read", "devices.read"],
            Some(&token),
        )
        .await
        .expect("hello");
    assert_eq!(r["status"], "ok");
    assert_eq!(r["api_version"], 1);
    assert_eq!(r["granted_scopes"], json!(["session.read", "devices.read"]));
}

#[tokio::test]
async fn spawn_token_allows_requesting_less() {
    let core = TestCore::start().await;
    let token = core.mint("tray", &["session.read", "devices.read"]);
    let mut c = core.connect().await;

    let r = c
        .hello("tray-official", "tray", &["session.read"], Some(&token))
        .await
        .expect("hello");
    assert_eq!(r["granted_scopes"], json!(["session.read"]));

    // The scopes not requested are not granted for all that.
    let err = c
        .request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn spawn_token_denies_scopes_beyond_mint() {
    let core = TestCore::start().await;
    let token = core.mint("tray", &["session.read"]);
    let mut c = core.connect().await;

    let err = c
        .hello(
            "tray-official",
            "tray",
            &["session.read", "devices.read"],
            Some(&token),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");

    // A refused hello consumes neither the token nor the connection.
    let r = c
        .hello("tray-official", "tray", &["session.read"], Some(&token))
        .await
        .expect("corrected hello on the same connection");
    assert_eq!(r["status"], "ok");
}

#[tokio::test]
async fn spawn_token_is_single_use() {
    let core = TestCore::start().await;
    let token = core.mint("tray", &["session.read"]);

    let mut first = core.connect().await;
    first
        .hello("tray-official", "tray", &["session.read"], Some(&token))
        .await
        .expect("first hello");

    let mut second = core.connect().await;
    let err = second
        .hello("impostor", "tray", &["session.read"], Some(&token))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "INVALID_TOKEN");
}

#[tokio::test]
async fn spawn_token_role_must_match_mint() {
    let core = TestCore::start().await;
    let token = core.mint("tray", &["session.read"]);
    let mut c = core.connect().await;

    let err = c
        .hello("tray-official", "custom", &["session.read"], Some(&token))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "INVALID_TOKEN");
}

#[tokio::test]
async fn file_token_grants_everything() {
    let core = TestCore::start().await;
    // gui() reads `ipc-token` and requests components.approve — trust root A
    // must be able to grant everything.
    let mut g = gui(&core).await;
    g.request("components.pending", json!({}))
        .await
        .expect("components.pending with the approve scope");
}

#[cfg(unix)]
#[tokio::test]
async fn file_token_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let core = TestCore::start().await;
    let meta = std::fs::metadata(core.config_dir().join("ipc-token")).expect("ipc-token absent");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o600,
        "the file token must be 0600"
    );
}

#[tokio::test]
async fn unknown_token_is_rejected() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    let err = c
        .hello("intruder", "custom", &["devices.read"], Some("t_bogus"))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "INVALID_TOKEN");
}

#[tokio::test]
async fn method_before_hello_is_rejected() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    let err = c.request("session.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "NOT_ENROLLED");
}

#[tokio::test]
async fn second_hello_after_success_is_rejected() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let token = core.mint("tray", &["session.read"]);
    let err = c
        .hello("tray-official", "tray", &["session.read"], Some(&token))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32600);
}

#[tokio::test]
async fn unknown_role_is_invalid_params() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    let err = c
        .hello("third-party", "supervisor", &["devices.read"], None)
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn unknown_scope_is_invalid_params() {
    let core = TestCore::start().await;
    let mut c = core.connect().await;

    let err = c
        .hello("third-party", "custom", &["devices.read", "coffee.read"], None)
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn clipboard_backend_role_is_exclusive() {
    let core = TestCore::start().await;
    let a = spawn_component(
        &core,
        "clip-official",
        "clipboard-backend",
        &["devices.read", "clipboard.read", "clipboard.write"],
    )
    .await;

    let token_b = core.mint("clipboard-backend", &["clipboard.read"]);
    let mut b = core.connect().await;
    let err = b
        .hello(
            "clip-third-party",
            "clipboard-backend",
            &["clipboard.read"],
            Some(&token_b),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "ROLE_CONFLICT");

    // The slot frees up when the holder disconnects; the refused token was not
    // consumed.
    drop(a);
    eventually(
        async || {
            matches!(
                b.hello("clip-third-party", "clipboard-backend", &["clipboard.read"], Some(&token_b))
                    .await,
                Ok(v) if v["status"] == "ok"
            )
        },
        "clipboard-backend role taken over after the holder disconnects",
    )
    .await;
}
