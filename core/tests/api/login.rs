// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! OIDC login (authorization code + PKCE, loopback) and device enrollment
//! (doc/core-api.md "session.*", doc/server-api.md "Enrollment") — against the
//! real server and the fake OIDC, with the harness playing the browser.

use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn login_requires_session_manage() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let err = c.request("session.login", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn login_without_server_config_is_unreachable() {
    // Core never configured: there is nowhere to log in.
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "gui-lite", "custom", &["session.manage"]).await;

    let err = c.request("session.login", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SERVER_UNREACHABLE");
}

#[tokio::test]
async fn login_with_broken_oidc_is_unreachable() {
    // The issuer responds but without a discovery document: no authorization
    // URL possible.
    let server = TestServer::start().await;
    let core = TestCore::start_with_config(universallink_core::ServerConfig {
        url: server.core_url(),
        oidc_issuer: format!("{}/not-found", server.oidc.issuer()),
        oidc_client_id: TEST_CLIENT_ID.into(),
        oidc_client_secret: None,
    })
    .await;
    let mut c = spawn_component(&core, "gui-lite", "custom", &["session.manage"]).await;

    let err = c.request("session.login", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SERVER_UNREACHABLE");
}

#[tokio::test]
async fn login_flow_establishes_session() {
    let server = TestServer::start().await;
    let mut observer = server.online_device("PC-Other", "macos").await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage"],
    )
    .await;
    observer.conn.drain().await;
    c.request("events.subscribe", json!({ "topics": ["session"] }))
        .await
        .expect("events.subscribe");

    let r = c
        .request("session.login", json!({}))
        .await
        .expect("session.login");
    let auth_url = r["auth_url"].as_str().expect("auth_url");

    // The authorization URL: the IdP endpoint, PKCE S256, state, loopback.
    assert!(
        auth_url.starts_with(&format!("{}/authorize?", server.oidc.issuer())),
        "unexpected auth_url: {auth_url}"
    );
    let q = url_params(auth_url);
    assert_eq!(q["client_id"], TEST_CLIENT_ID);
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(!q["code_challenge"].is_empty(), "empty code_challenge");
    assert!(!q["state"].is_empty(), "empty state");
    assert!(
        q["redirect_uri"].starts_with("http://127.0.0.1:")
            && q["redirect_uri"].ends_with("/callback"),
        "unexpected redirect_uri: {}",
        q["redirect_uri"]
    );

    // As long as the browser has not come, nothing changes.
    let r = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["logged_in"], false);
    c.assert_silent().await;

    // The "browser" runs through the flow: authorize → loopback redirect. The
    // page only responds once the enrollment is untangled.
    let page = browse(auth_url).await.expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);

    // The session opens right away (logged_in), the connection converges after.
    let p = c.expect_notification("session.changed").await;
    assert_eq!(p["logged_in"], true);
    assert_eq!(p["account"], json!({ "email": TEST_EMAIL }));
    wait_server_connected(&mut c, true).await;

    // session.json has exactly the shape that startup knows how to re-read.
    let session: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(core.config_dir().join("session.json")).expect("session.json"),
    )
    .expect("session.json en JSON");
    assert_eq!(session["server_url"], json!(server.core_url()));
    assert_eq!(session["account"], json!({ "email": TEST_EMAIL }));
    let device_id = session["device_id"].as_str().expect("device_id");

    // The observer has seen the enrollment then the connection — with the
    // Core's `device.key` identity (node_id = public key of the seed).
    let p = observer.conn.wait_notification("device.added").await;
    assert_eq!(p["device"]["device_id"], device_id);
    assert_eq!(p["device"]["name"], CORE_DEVICE_NAME);
    assert_eq!(p["device"]["platform"], std::env::consts::OS);
    let seed = std::fs::read_to_string(core.config_dir().join("device.key")).expect("device.key");
    assert_eq!(
        p["device"]["node_id"],
        json!(DeviceKey::from_seed_hex(seed.trim()).node_id())
    );
    let p = observer.conn.wait_notification("device.online").await;
    assert_eq!(p["device"]["device_id"], device_id);

    // And the refresh token waits for sensitive operations in the keyring —
    // which is readable only by us.
    assert!(
        core.secret("oidc-refresh-token").is_some(),
        "refresh token missing from the keyring"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(core.config_dir().join("secrets.json"))
            .expect("stat secrets.json")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "secrets.json must be 0600");
    }
}

#[tokio::test]
async fn login_without_email_claim_has_no_account() {
    let server = TestServer::start().await;
    // The IdP does not emit the email claim (scope refused by the user...):
    // the session opens anyway, without an `account`, anywhere.
    server.oidc.set_user_without_email(TEST_SUB);
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage"],
    )
    .await;
    c.request("events.subscribe", json!({ "topics": ["session"] }))
        .await
        .expect("events.subscribe");

    let r = c
        .request("session.login", json!({}))
        .await
        .expect("session.login");
    let page = browse(r["auth_url"].as_str().expect("auth_url"))
        .await
        .expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);

    let p = c.expect_notification("session.changed").await;
    assert_eq!(p["logged_in"], true);
    assert!(
        p.get("account").is_none(),
        "no email claim → no account: {p}"
    );
    let session: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(core.config_dir().join("session.json")).expect("session.json"),
    )
    .expect("session.json en JSON");
    assert!(
        session.get("account").is_none(),
        "session.json without account: {session}"
    );
}

#[tokio::test]
async fn broken_idp_reply_is_an_error_not_a_panic() {
    // An "IdP" that responds with a Latin-1 error page (non-UTF-8 body,
    // Content-Length exact in bytes — the lot of proxies and captive portals,
    // v1 being over plain http): the login fails cleanly.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake IdP");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = conn.read(&mut buf).await;
                let _ = conn
                    .write_all(
                        b"HTTP/1.1 500 Err\r\nContent-Length: 4\r\nConnection: close\r\n\r\ncaf\xE9",
                    )
                    .await;
            });
        }
    });

    let core = TestCore::start_with_config(universallink_core::ServerConfig {
        url: format!("ws://{addr}/ws"),
        oidc_issuer: format!("http://{addr}"),
        oidc_client_id: TEST_CLIENT_ID.into(),
        oidc_client_secret: None,
    })
    .await;
    let mut c = spawn_component(&core, "gui-lite", "custom", &["session.manage"]).await;

    let err = c.request("session.login", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SERVER_UNREACHABLE");
}

#[tokio::test]
async fn login_when_logged_in_is_rejected() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(&core, "gui-lite", "custom", &["session.manage"]).await;

    let err = c.request("session.login", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "ALREADY_LOGGED_IN");
}

#[tokio::test]
async fn relogin_after_logout_works() {
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
    c.request("session.logout", json!({}))
        .await
        .expect("session.logout");
    // The flow replays from scratch — new session (and new device: the
    // re-login re-enrolls, the old entry stays in the directory).
    complete_login(&mut c).await;
}

#[tokio::test]
async fn second_login_replaces_first_flow() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage"],
    )
    .await;

    let first = c
        .request("session.login", json!({}))
        .await
        .expect("login 1")["auth_url"]
        .as_str()
        .expect("auth_url")
        .to_string();
    let second = c
        .request("session.login", json!({}))
        .await
        .expect("login 2")["auth_url"]
        .as_str()
        .expect("auth_url")
        .to_string();
    assert_ne!(first, second, "each flow has its own state and verifier");

    // The first flow is dead: its listener disappears (the IdP's redirect leads
    // nowhere).
    eventually(
        async || browse(&first).await.is_err(),
        "death of the replaced flow",
    )
    .await;

    // The second succeeds.
    let page = browse(&second).await.expect("second flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);
    wait_server_connected(&mut c, true).await;
}

#[tokio::test]
async fn callback_rejects_forged_requests_without_killing_the_flow() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage"],
    )
    .await;

    let r = c
        .request("session.login", json!({}))
        .await
        .expect("session.login");
    let auth_url = r["auth_url"].as_str().expect("auth_url");
    let redirect = url_params(auth_url)["redirect_uri"].clone();

    // A stolen code without the right state: rejected.
    let page = http_get(&format!("{redirect}?code=stolen&state=forged"))
        .await
        .expect("loopback response");
    assert_eq!(page.status, 400, "page: {}", page.body);
    // Some arbitrary path: nothing here.
    let base = redirect.trim_end_matches("/callback").to_string();
    let page = http_get(&format!("{base}/favicon.ico"))
        .await
        .expect("loopback response");
    assert_eq!(page.status, 404, "page: {}", page.body);

    // None of these requests consumed the flow: the real one succeeds.
    let page = browse(auth_url).await.expect("real flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);
    wait_server_connected(&mut c, true).await;
}

#[tokio::test]
async fn login_denied_by_user_ends_flow() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage"],
    )
    .await;

    let r = c
        .request("session.login", json!({}))
        .await
        .expect("session.login");
    let auth_url = r["auth_url"].as_str().expect("auth_url");
    let q = url_params(auth_url);

    // The user refuses: the IdP comes back with `error` instead of `code`.
    let denial = format!(
        "{}?error=access_denied&state={}",
        q["redirect_uri"], q["state"]
    );
    let page = http_get(&denial).await.expect("loopback response");
    assert_eq!(page.status, 403, "page: {}", page.body);

    // No session, and the flow is consumed: the listener disappears.
    eventually(
        async || http_get(&denial).await.is_err(),
        "death of the refused flow",
    )
    .await;
    let r = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["logged_in"], false);
}

#[tokio::test]
async fn login_failure_reaches_the_browser() {
    // The server goes down between the URL and the browser: the OIDC exchange
    // succeeds but the enrollment fails — the page says so, the session does
    // not open.
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage"],
    )
    .await;

    let r = c
        .request("session.login", json!({}))
        .await
        .expect("session.login");
    let auth_url = r["auth_url"].as_str().expect("auth_url");
    server.cut();

    let page = browse(auth_url).await.expect("loopback response");
    assert_eq!(page.status, 502, "page: {}", page.body);
    let r = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["logged_in"], false);
    assert!(
        !core.config_dir().join("session.json").exists(),
        "no session.json after a failed login"
    );
}
