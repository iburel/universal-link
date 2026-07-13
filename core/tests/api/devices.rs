// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Device directory, IPC side: cached snapshot enriched with `is_self`,
//! `devices.rename` proxy, relaying of `device.*` notifications on the
//! `devices` topic (doc/core-api.md "devices.*", doc/server-api.md
//! "Notifications").

use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn list_marks_self() {
    let server = TestServer::start().await;
    let other = server.online_device("PC-Other", "macos").await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "menu-official",
        "menu-backend",
        &["session.read", "devices.read"],
    )
    .await;
    wait_server_connected(&mut c, true).await;

    // Connected ⇒ primed cache: the snapshot is available with no further wait.
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(list.as_array().expect("list").len(), 2);

    let own = find_device(&list, core.device_id());
    assert_eq!(own["is_self"], true);
    assert_eq!(own["name"], CORE_DEVICE_NAME);
    assert_eq!(own["online"], true);
    assert_eq!(own["node_id"], core.key().node_id());

    let o = find_device(&list, &other.device_id);
    assert_eq!(o["is_self"], false);
    assert_eq!(o["online"], true);
}

#[tokio::test]
async fn list_requires_devices_read() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let err = c.request("devices.list", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn list_without_session_is_unreachable() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "menu-official", "menu-backend", &["devices.read"]).await;

    let err = c.request("devices.list", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SERVER_UNREACHABLE");
}

#[tokio::test]
async fn list_before_first_sync_is_unreachable() {
    let server = TestServer::start().await;
    // Session on disk, server unreachable from the start: there is no
    // "last known state" to serve — lying is worse than refusing.
    let core = TestCore::start_enrolled_server_cut(&server).await;
    let mut c = spawn_component(
        &core,
        "menu-official",
        "menu-backend",
        &["session.read", "devices.read"],
    )
    .await;

    let r = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["logged_in"], true);
    assert_eq!(r["server_connected"], false);

    let err = c.request("devices.list", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SERVER_UNREACHABLE");
}

#[tokio::test]
async fn list_serves_cache_when_server_lost() {
    let server = TestServer::start().await;
    let _other = server.online_device("PC-Other", "macos").await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "menu-official",
        "menu-backend",
        &["session.read", "devices.read"],
    )
    .await;
    wait_server_connected(&mut c, true).await;

    server.cut();
    wait_server_connected(&mut c, false).await;

    // The last known snapshot is still served — freshness is read from
    // session.changed / session.status, not from a refusal.
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list while offline");
    assert_eq!(list.as_array().expect("list").len(), 2);
    assert_eq!(find_device(&list, core.device_id())["is_self"], true);
}

#[tokio::test]
async fn rename_proxies_and_relays() {
    let server = TestServer::start().await;
    let mut other = server.online_device("PC-Other", "macos").await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "devices.read", "devices.manage"],
    )
    .await;
    wait_server_connected(&mut c, true).await;
    other.conn.drain().await;
    c.request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .expect("events.subscribe");

    let r = c
        .request(
            "devices.rename",
            json!({ "device_id": other.device_id, "name": "PC-Renamed" }),
        )
        .await
        .expect("devices.rename");
    assert_eq!(r["device"]["name"], "PC-Renamed");
    assert_eq!(r["device"]["is_self"], false);

    // The server has broadcast to the other devices of the account.
    let p = other.conn.wait_notification("device.updated").await;
    assert_eq!(p["device"]["name"], "PC-Renamed");

    // The IPC subscribers too: the Core synthesizes the event that the server
    // does not send back to it (it is the requester).
    let p = c.expect_notification("device.updated").await;
    assert_eq!(p["device"]["device_id"], other.device_id);
    assert_eq!(p["device"]["name"], "PC-Renamed");
    assert_eq!(p["device"]["is_self"], false);

    // And the cache is up to date.
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(find_device(&list, &other.device_id)["name"], "PC-Renamed");
}

#[tokio::test]
async fn rename_requires_devices_manage() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "menu-official", "menu-backend", &["devices.read"]).await;

    let err = c
        .request("devices.rename", json!({ "device_id": "d_x", "name": "X" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn rename_relays_server_errors() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "devices.manage"],
    )
    .await;
    wait_server_connected(&mut c, true).await;

    // The server error passes through the proxy as-is (code + data.code).
    let err = c
        .request(
            "devices.rename",
            json!({ "device_id": "d_unknown", "name": "X" }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");
}

#[tokio::test]
async fn rename_disconnected_is_unreachable() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "devices.manage"],
    )
    .await;
    wait_server_connected(&mut c, true).await;
    server.cut();
    wait_server_connected(&mut c, false).await;

    let err = c
        .request("devices.rename", json!({ "device_id": "d_x", "name": "X" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SERVER_UNREACHABLE");
}

#[tokio::test]
async fn relays_device_lifecycle() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "menu-official",
        "menu-backend",
        &["session.read", "devices.read"],
    )
    .await;
    wait_server_connected(&mut c, true).await;
    c.request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .expect("events.subscribe");

    // Enrollment of a new device → device.added (offline: not yet
    // authenticated).
    let mut newcomer = server.enrolled_device("PC-Three", "windows").await;
    let p = c.expect_notification("device.added").await;
    assert_eq!(p["device"]["device_id"], newcomer.device_id);
    assert_eq!(p["device"]["online"], false);
    assert_eq!(p["device"]["is_self"], false);

    // Authentication → device.online.
    authenticate(&mut newcomer.conn, &newcomer.key, &newcomer.device_id).await;
    let p = c.expect_notification("device.online").await;
    assert_eq!(p["device"]["device_id"], newcomer.device_id);
    assert_eq!(p["device"]["online"], true);

    // Disconnection → device.offline { device_id, last_seen } (server payload).
    drop(newcomer.conn);
    let p = c.expect_notification("device.offline").await;
    assert_eq!(p["device_id"], newcomer.device_id);
    assert_rfc3339(&p["last_seen"]);

    // The cache has followed each event.
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let d = find_device(&list, &newcomer.device_id);
    assert_eq!(d["online"], false);
    assert_eq!(d["is_self"], false);
}

#[tokio::test]
async fn relays_device_removed() {
    let server = TestServer::start().await;
    let mut admin = server.online_device("PC-Admin", "macos").await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "menu-official",
        "menu-backend",
        &["session.read", "devices.read"],
    )
    .await;
    wait_server_connected(&mut c, true).await;
    c.request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .expect("events.subscribe");

    let victim = server.enrolled_device("PC-Doomed", "windows").await;
    c.expect_notification("device.added").await;

    admin
        .conn
        .request(
            "devices.revoke",
            json!({
                "device_id": victim.device_id,
                "id_token": server.oidc.id_token(TEST_SUB),
            }),
        )
        .await
        .expect("devices.revoke");

    let p = c.expect_notification("device.removed").await;
    assert_eq!(p["device_id"], victim.device_id);

    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert!(
        list.as_array()
            .expect("list")
            .iter()
            .all(|d| d["device_id"] != victim.device_id),
        "revoked device still in cache: {list}"
    );
}

#[tokio::test]
async fn no_relay_without_subscription() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "menu-official",
        "menu-backend",
        &["session.read", "devices.read"],
    )
    .await;
    wait_server_connected(&mut c, true).await;

    // The scope would allow the subscription, but it did not happen: silence.
    let _newcomer = server.enrolled_device("PC-Three", "windows").await;
    c.assert_silent().await;
}

#[tokio::test]
async fn topics_filter_what_is_relayed() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "tray-official",
        "tray",
        &["session.read", "devices.read"],
    )
    .await;
    wait_server_connected(&mut c, true).await;
    // Subscribed to `session` only: the device.* do not get through.
    c.request("events.subscribe", json!({ "topics": ["session"] }))
        .await
        .expect("events.subscribe");

    let _newcomer = server.enrolled_device("PC-Three", "windows").await;
    c.assert_silent().await;
}

// -- devices.revoke ----------------------------------------------------------

#[tokio::test]
async fn revoke_requires_devices_manage() {
    let core = TestCore::start().await;
    let mut c = spawn_component(
        &core,
        "tray-official",
        "tray",
        &["session.read", "devices.read"],
    )
    .await;

    let err = c
        .request("devices.revoke", json!({ "device_id": "d_x" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn revoke_with_refresh_token_is_done() {
    let server = TestServer::start().await;
    let mut victim = server.online_device("PC-Victim", "windows").await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &[
            "session.read",
            "session.manage",
            "devices.read",
            "devices.manage",
        ],
    )
    .await;
    // The login stashes a refresh token: revocation will not need a browser.
    complete_login(&mut c).await;
    // The victim has seen the Core enroll and connect: start over from silence,
    // to check that the revocation, for its part, arrives without a word.
    victim.conn.drain().await;
    c.request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .expect("events.subscribe");

    let r = c
        .request("devices.revoke", json!({ "device_id": victim.device_id }))
        .await
        .expect("devices.revoke");
    assert_eq!(r, json!({ "status": "done" }));

    // The victim is closed without a word (DEVICE_REVOKED), the IPC subscribers
    // see device.removed (synthesized: the server excludes the requester), and
    // the cache follows.
    let close = victim
        .conn
        .expect_close_silent()
        .await
        .expect("close frame");
    assert_eq!(close.1, "DEVICE_REVOKED");
    let p = c.wait_notification("device.removed").await;
    assert_eq!(p["device_id"], victim.device_id);
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert!(
        list.as_array()
            .expect("list")
            .iter()
            .all(|d| d["device_id"] != victim.device_id),
        "the revoked device must leave the cache: {list}"
    );
}

#[tokio::test]
async fn revoke_unknown_device_relays_error() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage", "devices.manage"],
    )
    .await;
    complete_login(&mut c).await;

    let err = c
        .request("devices.revoke", json!({ "device_id": "d_unknown" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");
}

#[tokio::test]
async fn revoke_without_refresh_token_needs_reauth() {
    let server = TestServer::start().await;
    let mut victim = server.online_device("PC-Victim", "linux").await;
    // Seeded session: no refresh token in the keyring — the case of an older
    // session, or of a lost keyring.
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "devices.read", "devices.manage"],
    )
    .await;
    wait_server_connected(&mut c, true).await;
    // The victim has seen the Core arrive: start over from silence.
    victim.conn.drain().await;
    c.request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .expect("events.subscribe");

    let r = c
        .request("devices.revoke", json!({ "device_id": victim.device_id }))
        .await
        .expect("devices.revoke");
    assert_eq!(r["status"], "reauth_required");
    let auth_url = r["auth_url"].as_str().expect("auth_url");

    // Re-authentication in the browser carries out the revocation.
    let page = browse(auth_url).await.expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);
    let p = c.wait_notification("device.removed").await;
    assert_eq!(p["device_id"], victim.device_id);
    let close = victim
        .conn
        .expect_close_silent()
        .await
        .expect("close frame");
    assert_eq!(close.1, "DEVICE_REVOKED");

    // The fresh refresh token from the flow is stashed: the next revocation
    // will do without a browser.
    assert!(
        core.secret("oidc-refresh-token").is_some(),
        "re-auth refresh token missing from the keyring"
    );
}

#[tokio::test]
async fn revoke_with_dead_refresh_token_needs_reauth() {
    let server = TestServer::start().await;
    let victim = server.online_device("PC-Victim", "macos").await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage", "devices.manage"],
    )
    .await;
    complete_login(&mut c).await;

    // The IdP no longer recognizes the refresh token (expired, revoked):
    // re-auth, and the dead secret is thrown out of the keyring.
    server.oidc.revoke_refresh_tokens();
    let r = c
        .request("devices.revoke", json!({ "device_id": victim.device_id }))
        .await
        .expect("devices.revoke");
    assert_eq!(r["status"], "reauth_required");
    assert!(
        core.secret("oidc-refresh-token").is_none(),
        "a dead refresh token must not stay in the keyring"
    );
}

#[tokio::test]
async fn revoke_self_drops_session() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &[
            "session.read",
            "session.manage",
            "devices.read",
            "devices.manage",
        ],
    )
    .await;
    complete_login(&mut c).await;
    c.request(
        "events.subscribe",
        json!({ "topics": ["session", "devices"] }),
    )
    .await
    .expect("events.subscribe");
    let own = own_device_id(&mut c).await;

    // Self-revoking is allowed: the response arrives before the close.
    let r = c
        .request("devices.revoke", json!({ "device_id": own }))
        .await
        .expect("devices.revoke");
    assert_eq!(r, json!({ "status": "done" }));

    // device.removed first (the order of the server stream), then the session
    // drops (DEVICE_REVOKED close → abandonment).
    let p = c.wait_notification("device.removed").await;
    assert_eq!(p["device_id"], own);
    let p = c.wait_notification("session.changed").await;
    assert_eq!(p["logged_in"], false);
    eventually(
        async || !core.config_dir().join("session.json").exists(),
        "deletion of session.json after self-revocation",
    )
    .await;
    assert!(
        core.secret("oidc-refresh-token").is_none(),
        "the refresh token leaves with the session"
    );
}

#[tokio::test]
async fn revoke_disconnected_is_unreachable() {
    let server = TestServer::start().await;
    let victim = server.enrolled_device("PC-Victim", "windows").await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "devices.manage"],
    )
    .await;
    wait_server_connected(&mut c, true).await;

    server.cut();
    wait_server_connected(&mut c, false).await;
    let err = c
        .request("devices.revoke", json!({ "device_id": victim.device_id }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SERVER_UNREACHABLE");
}

#[tokio::test]
async fn refresh_token_survives_restart() {
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

    // Restart: the session (written by the real login) AND the keyring
    // survive — the revocation still does without a browser.
    let core = core.restart().await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "devices.manage"],
    )
    .await;
    wait_server_connected(&mut c, true).await;
    let victim = server.online_device("PC-Victim", "linux").await;
    let r = c
        .request("devices.revoke", json!({ "device_id": victim.device_id }))
        .await
        .expect("devices.revoke");
    assert_eq!(r, json!({ "status": "done" }));
}

#[tokio::test]
async fn revoke_with_stale_id_token_needs_reauth() {
    let server = TestServer::start().await;
    let mut victim = server.online_device("PC-Victim", "windows").await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &[
            "session.read",
            "session.manage",
            "devices.read",
            "devices.manage",
        ],
    )
    .await;
    complete_login(&mut c).await;
    victim.conn.drain().await;
    c.request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .expect("events.subscribe");

    // The refresh grant returns aged tokens: valid, but that the server deems
    // too old for a sensitive operation (OIDC_INVALID). The Core then switches
    // to re-auth instead of relaying the error.
    server.oidc.stale_refresh_grants();
    let r = c
        .request("devices.revoke", json!({ "device_id": victim.device_id }))
        .await
        .expect("devices.revoke");
    assert_eq!(r["status"], "reauth_required");

    // And the re-auth succeeds: the fresh code from the browser is authoritative.
    let page = browse(r["auth_url"].as_str().expect("auth_url"))
        .await
        .expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);
    let p = c.wait_notification("device.removed").await;
    assert_eq!(p["device_id"], victim.device_id);
    let close = victim
        .conn
        .expect_close_silent()
        .await
        .expect("close frame");
    assert_eq!(close.1, "DEVICE_REVOKED");
}

#[tokio::test]
async fn logout_cancels_pending_reauth_flow() {
    let server = TestServer::start().await;
    let mut victim = server.online_device("PC-Victim", "macos").await;
    // Seeded session: no refresh token, the revocation will go through
    // re-auth.
    let core = TestCore::start_enrolled(&server).await;
    let mut c = spawn_component(
        &core,
        "gui-lite",
        "custom",
        &["session.read", "session.manage", "devices.manage"],
    )
    .await;
    wait_server_connected(&mut c, true).await;
    victim.conn.drain().await;

    let r = c
        .request("devices.revoke", json!({ "device_id": victim.device_id }))
        .await
        .expect("devices.revoke");
    assert_eq!(r["status"], "reauth_required");
    let auth_url = r["auth_url"].as_str().expect("auth_url").to_string();

    // The re-auth flow belongs to the session: the logout carries it away with
    // it — the tab left open will no longer be able to revoke or stash anything.
    c.request("session.logout", json!({}))
        .await
        .expect("session.logout");
    eventually(
        async || browse(&auth_url).await.is_err(),
        "death of the re-auth flow on logout",
    )
    .await;
    assert!(
        core.secret("oidc-refresh-token").is_none(),
        "nothing must enter the keyring after logout"
    );
    // The victim has seen the Core leave (logout) — and nothing else: it was
    // not revoked.
    let p = victim.conn.wait_notification("device.offline").await;
    assert_eq!(p["device_id"], core.device_id());
    victim.conn.assert_silent().await;
}
