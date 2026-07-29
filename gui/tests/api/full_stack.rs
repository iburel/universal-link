// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The full chain, all in-process: webview (mock) → commands → IPC client →
//! real Core → real server + FakeOidc. The nominal GUI scenario.

use serde_json::json;

use crate::support::*;

#[tokio::test(flavor = "multi_thread")]
async fn login_and_devices_through_the_shell() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut shell = shell_app(gui_config(&core)).await;
    shell.wait_status("connected").await;

    // Login from the webview: the shell returns the auth_url — it's the
    // frontend that will open the browser (opener plugin, outside the
    // MockRuntime scope). The harness's "browser" runs through the flow.
    let r = shell
        .core_request("session.login", json!({}))
        .await
        .expect("session.login");
    let auth_url = r["auth_url"].as_str().expect("auth_url");
    let page = browse(auth_url).await.expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);

    // The completion arrives as a webview notification; we wait for the
    // active server session (logged_in first, server_connected next).
    loop {
        let p = shell.wait_core_notification("session.changed").await;
        assert_eq!(p["logged_in"], true, "{p}");
        if p["server_connected"] == json!(true) {
            assert_eq!(p["account"]["email"], TEST_EMAIL);
            break;
        }
    }

    // The directory, seen from the webview: the Core sees itself in it, online.
    let list = shell
        .core_request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let list = list.as_array().expect("list of devices");
    let own = list
        .iter()
        .find(|d| d["is_self"] == json!(true))
        .unwrap_or_else(|| panic!("no is_self device: {list:?}"));
    assert_eq!(own["name"], CORE_DEVICE_NAME);
    assert_eq!(own["online"], true);

    // Logout: the session closes, the webview sees it.
    shell
        .core_request("session.logout", json!({}))
        .await
        .expect("session.logout");
    let p = shell.wait_core_notification("session.changed").await;
    assert_eq!(p["logged_in"], false);
    assert_eq!(p["server_connected"], false);
}

/// First run, through the shell: an address, and the settings the setup screen no
/// longer asks for come back from the server's own descriptor and land in
/// `config.json`. The three steps the screen strings together — `session.discover`,
/// the shell's write, `session.reload`.
///
/// What this cannot show is `configured` flipping: this harness's Core carries a
/// fixed `reload_server` closure (it does not read the shell's file, and the two
/// keep separate folders), so a reload here re-applies what it was built with.
/// That effect is covered where it is expressible, on a real closure —
/// `core/tests/api/session.rs::reload_configures_an_unconfigured_core`.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_configures_itself_from_the_server_address() {
    let server = TestServer::start().await;
    // Nothing configured: the state a fresh install starts from.
    let core = TestCore::start().await;
    let shell = shell_app(gui_config(&core)).await;
    shell.wait_status("connected").await;
    assert_eq!(shell.get_server_config().await["server_url"], "");

    // One address — as it would be pasted, path and all.
    let found = shell
        .core_request("session.discover", json!({ "url": server.url() }))
        .await
        .expect("session.discover");
    assert_eq!(found["server_url"], server.url());
    assert_eq!(found["oidc_issuer"], server.oidc.issuer());
    assert!(
        found["oidc_client_id"].is_string(),
        "the client id the deployment publishes: {found}"
    );

    // The shell writes what came back — it owns config.json — and the Core takes
    // the sequence without complaint.
    shell
        .set_server_config(found.clone())
        .await
        .expect("set_server_config");
    shell
        .core_request("session.reload", json!({}))
        .await
        .expect("session.reload");

    // Written, from an address alone: what used to take three fields.
    let written = shell.get_server_config().await;
    assert_eq!(written["server_url"], server.url());
    assert_eq!(written["oidc_issuer"], server.oidc.issuer());
    assert_eq!(written["oidc_client_id"], found["oidc_client_id"]);
}
