// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Server session: device identity, status, connection/reconnection to the
//! real server (universallink-server lib, in-process), logout, revocation
//! (doc/core-api.md "session.*", doc/server-api.md "Lifecycle").
//! The OIDC login is the next building block: the harness seeds the identity.

use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn status_starts_logged_out() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let r = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["logged_in"], false);
    assert_eq!(r["server_connected"], false);
    assert!(
        r.get("account").is_none(),
        "no account outside a session: {r}"
    );
}

#[tokio::test]
async fn status_requires_session_read() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "menu-official", "menu-backend", &["devices.read"]).await;

    let err = c.request("session.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn device_key_survives_restart() {
    let core = TestCore::start().await;

    // Generated at first startup, even without a session: it is the device's
    // identity (and iroh's), it precedes the login.
    let path = core.config_dir().join("device.key");
    let seed = std::fs::read_to_string(&path).expect("device.key");
    assert_eq!(seed.trim().len(), 64, "hex seed of 32 bytes: {seed:?}");
    hex::decode(seed.trim()).expect("device.key in hex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "device.key must be 0600");
    }

    // A restart re-reads it instead of generating another.
    let core = core.restart().await;
    let again = std::fs::read_to_string(core.config_dir().join("device.key")).expect("device.key");
    assert_eq!(again, seed, "the identity must survive the restart");
}

#[tokio::test]
async fn status_reflects_active_session() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    // The session is on disk: logged_in immediately, without waiting for the
    // server.
    let r = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["logged_in"], true);
    assert_eq!(r["account"], json!({ "email": TEST_EMAIL }));

    // The connection converges on its own (challenge + authenticate + snapshot).
    wait_server_connected(&mut c, true).await;
}

#[tokio::test]
async fn session_changed_on_server_loss_and_return() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;
    wait_server_connected(&mut c, true).await;
    c.request("events.subscribe", json!({ "topics": ["session"] }))
        .await
        .expect("events.subscribe");

    server.cut();
    let p = c.wait_notification("session.changed").await;
    assert_eq!(p["server_connected"], false);
    assert_eq!(
        p["logged_in"], true,
        "losing the server ≠ losing the session"
    );
    assert_eq!(p["account"], json!({ "email": TEST_EMAIL }));

    server.restore();
    let p = c.wait_notification("session.changed").await;
    assert_eq!(p["server_connected"], true);
}

#[tokio::test]
async fn replaced_connection_is_taken_back() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;
    wait_server_connected(&mut c, true).await;

    // Another process authenticates with the same identity: the server
    // replaces the Core's connection (`REPLACED`).
    let mut intruder = server.connect_direct().await;
    authenticate(&mut intruder, core.key(), core.device_id()).await;

    // The Core reconnects — and replaces the intruder in turn.
    let close = intruder.expect_close().await.expect("close frame");
    assert_eq!(close.1, "REPLACED");
    wait_server_connected(&mut c, true).await;
}

#[tokio::test]
async fn logout_closes_session() {
    let server = TestServer::start().await;
    let mut observer = server.online_device("PC-Other", "macos").await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage", "devices.read"],
    )
    .await;
    wait_server_connected(&mut c, true).await;
    observer.conn.drain().await;
    c.request("events.subscribe", json!({ "topics": ["session"] }))
        .await
        .expect("events.subscribe");

    let r = c
        .request("session.logout", json!({}))
        .await
        .expect("session.logout");
    assert_eq!(r, json!({}));

    // A single transition, everything drops at once.
    let p = c.expect_notification("session.changed").await;
    assert_eq!(p["logged_in"], false);
    assert_eq!(p["server_connected"], false);
    assert!(p.get("account").is_none(), "no more account: {p}");

    // The server sees the device leave — and it does not come back.
    let p = observer.conn.wait_notification("device.offline").await;
    assert_eq!(p["device_id"], core.device_id());
    observer.conn.assert_silent().await;

    // The session survives neither in memory nor on disk.
    assert!(
        !core.config_dir().join("session.json").exists(),
        "session.json must be deleted on logout"
    );
    let r = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["logged_in"], false);
    let err = c.request("devices.list", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SERVER_UNREACHABLE");
}

#[tokio::test]
async fn logout_forgets_refresh_token() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage"],
    )
    .await;
    complete_login(&mut c).await;
    assert!(
        core.secret("oidc-refresh-token").is_some(),
        "the login must stash a refresh token"
    );

    c.request("session.logout", json!({}))
        .await
        .expect("session.logout");
    // The refresh token belonged to the session: it leaves with it.
    assert!(
        core.secret("oidc-refresh-token").is_none(),
        "the refresh token must leave on logout"
    );
}

#[tokio::test]
async fn logout_requires_session_manage() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let err = c.request("session.logout", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn logout_when_logged_out_is_noop() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "gui-lite", "custom", &["session.manage"]).await;

    let r = c
        .request("session.logout", json!({}))
        .await
        .expect("session.logout outside a session");
    assert_eq!(r, json!({}));
}

#[tokio::test]
async fn status_reports_whether_a_server_is_configured() {
    // Unconfigured (fresh install): the flag is false — the GUI shows its
    // first-run setup screen rather than a connection error (both otherwise
    // read as `server_connected: false`).
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "gui-lite", "custom", &["session.read"]).await;
    let r = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["configured"], false, "no server configured yet: {r}");

    // Configured (server + OIDC set): the flag is true.
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(&core, "gui-lite", "custom", &["session.read"]).await;
    let r = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["configured"], true, "server configured: {r}");
}

#[tokio::test]
async fn reload_configures_an_unconfigured_core() {
    let core = TestCore::start().await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage"],
    )
    .await;
    assert_eq!(
        c.request("session.status", json!({}))
            .await
            .expect("status")["configured"],
        false
    );

    // The GUI has just written config.json: reload picks it up, no restart.
    core.stage_config(Some(universallink_core::ServerConfig {
        url: "wss://relay.example/ws".into(),
        oidc_issuer: "https://idp.example".into(),
        oidc_client_id: "public-id".into(),
        oidc_client_secret: None,
    }));
    let r = c
        .request("session.reload", json!({}))
        .await
        .expect("session.reload");
    assert_eq!(r["configured"], true, "reload must apply the config: {r}");
    // And it sticks for the next status read.
    assert_eq!(
        c.request("session.status", json!({}))
            .await
            .expect("status")["configured"],
        true
    );
}

#[tokio::test]
async fn reload_reports_an_invalid_config() {
    let core = TestCore::start().await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage"],
    )
    .await;
    // A half-filled config.json: the reason is surfaced, and the Core stays
    // unconfigured rather than silently swallowing it.
    core.stage_invalid_config("incomplete configuration: only server_url is set");
    let err = c.request("session.reload", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "INVALID_CONFIG");
    assert_eq!(
        c.request("session.status", json!({}))
            .await
            .expect("status")["configured"],
        false
    );
}

#[tokio::test]
async fn reload_requires_session_manage() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;
    let err = c.request("session.reload", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn revoked_core_drops_its_session() {
    let server = TestServer::start().await;
    let mut admin = server.online_device("PC-Admin", "macos").await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;
    wait_server_connected(&mut c, true).await;

    // Revocation from another PC: the server closes the Core's connection with
    // `DEVICE_REVOKED` — the session is dead, the enrollment to be redone;
    // reconnecting in a loop would be harassment.
    admin
        .conn
        .request(
            "devices.revoke",
            json!({
                "device_id": core.device_id(),
                "id_token": server.oidc.id_token(TEST_SUB),
            }),
        )
        .await
        .expect("devices.revoke");

    eventually(
        async || {
            let r = c
                .request("session.status", json!({}))
                .await
                .expect("session.status");
            r["logged_in"] == json!(false)
        },
        "abandonment of the session after revocation",
    )
    .await;
    assert!(
        !core.config_dir().join("session.json").exists(),
        "session.json must be deleted after revocation"
    );
}

#[tokio::test]
async fn revocation_while_offline_drops_session_at_reconnect() {
    let server = TestServer::start().await;
    let mut admin = server.online_device("PC-Admin", "macos").await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;
    wait_server_connected(&mut c, true).await;

    // The dominant case: revoking a powered-off/disconnected PC. The Core will
    // never see a close frame — it is the authenticate error on reconnection
    // that carries the verdict.
    server.cut();
    wait_server_connected(&mut c, false).await;
    admin
        .conn
        .request(
            "devices.revoke",
            json!({
                "device_id": core.device_id(),
                "id_token": server.oidc.id_token(TEST_SUB),
            }),
        )
        .await
        .expect("devices.revoke");
    server.restore();

    eventually(
        async || {
            let r = c
                .request("session.status", json!({}))
                .await
                .expect("session.status");
            r["logged_in"] == json!(false)
        },
        "abandonment of the session on reconnection of a revoked device",
    )
    .await;
    assert!(
        !core.config_dir().join("session.json").exists(),
        "session.json must be deleted after offline revocation"
    );
}
