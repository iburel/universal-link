// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The full chain, all in-process: client → real Core → real server +
//! FakeOidc. The nominal GUI scenario — login, live directory, logout.

use serde_json::json;

use crate::support::*;

const GUI_SCOPES: &[&str] = &[
    "session.read",
    "session.manage",
    "devices.read",
    "devices.manage",
];

#[tokio::test]
async fn full_login_through_the_client() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let (client, mut events) = connected(&core, "gui", GUI_SCOPES, &["session", "devices"]).await;

    // session.login → auth_url; the harness's "browser" runs the flow.
    let r = client
        .request("session.login", json!({}))
        .await
        .expect("session.login");
    let auth_url = r["auth_url"].as_str().expect("auth_url");
    let page = browse(auth_url).await.expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);

    // The session opens then converges: logged_in first, the server
    // connection next. The account comes from the ID token's email claim.
    let p = wait_notification(&mut events, "session.changed").await;
    assert_eq!(p["logged_in"], true);
    assert_eq!(p["account"]["email"], TEST_EMAIL);
    let mut p = p;
    while p["server_connected"] != json!(true) {
        p = wait_notification(&mut events, "session.changed").await;
        assert_eq!(p["logged_in"], true, "{p}");
    }

    // The directory is primed: the Core sees itself in it.
    let list = client
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let list = list.as_array().expect("device list");
    let own = list
        .iter()
        .find(|d| d["is_self"] == json!(true))
        .unwrap_or_else(|| panic!("no is_self device: {list:?}"));
    assert_eq!(own["name"], CORE_DEVICE_NAME);
    assert_eq!(own["online"], true);

    // Logout: a single notification, session closed.
    client
        .request("session.logout", json!({}))
        .await
        .expect("session.logout");
    let p = wait_notification(&mut events, "session.changed").await;
    assert_eq!(p["logged_in"], false);
    assert_eq!(p["server_connected"], false);
}

#[tokio::test]
async fn device_lifecycle_events_are_relayed() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let (client, mut events) = connected(&core, "gui", GUI_SCOPES, &["session", "devices"]).await;

    let r = client
        .request("session.login", json!({}))
        .await
        .expect("session.login");
    let page = browse(r["auth_url"].as_str().expect("auth_url"))
        .await
        .expect("browser flow");
    assert_eq!(page.status, 200);
    let mut p = wait_notification(&mut events, "session.changed").await;
    while p["server_connected"] != json!(true) {
        p = wait_notification(&mut events, "session.changed").await;
    }

    // Another PC on the account appears: enrollment then connection.
    let other = server.online_device("PC-B", "linux").await;
    let p = wait_notification(&mut events, "device.added").await;
    assert_eq!(p["device"]["device_id"], other.device_id);
    assert_eq!(p["device"]["is_self"], false);
    let p = wait_notification(&mut events, "device.online").await;
    assert_eq!(p["device"]["device_id"], other.device_id);

    // Rename via the client: enriched response + device.updated synthesized
    // for the IPC subscribers (the requester included).
    let r = client
        .request(
            "devices.rename",
            json!({ "device_id": other.device_id, "name": "PC-Beta" }),
        )
        .await
        .expect("devices.rename");
    assert_eq!(r["device"]["name"], "PC-Beta");
    let p = wait_notification(&mut events, "device.updated").await;
    assert_eq!(p["device"]["device_id"], other.device_id);
    assert_eq!(p["device"]["name"], "PC-Beta");

    // The PC disconnects: device.offline (server payload, no record).
    let other_id = other.device_id.clone();
    drop(other);
    let p = wait_notification(&mut events, "device.offline").await;
    assert_eq!(p["device_id"], other_id);
}
