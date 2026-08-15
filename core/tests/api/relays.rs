// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The deployment's relay announcement, end to end (#105): the server states
//! its relays in the deployment descriptor, a session picks them up, the Core
//! keeps its own copy and hands the list to the transport, and the word dies
//! with the relationship (logout). The switchboard's `announced` record is
//! the assertion mechanism: it is what the real transport would fold into
//! its next bind.

use onedevice_test_support::memory_transport::MemorySwitchboard;
use serde_json::json;

use crate::support::*;

const ANNOUNCED: [&str; 2] = ["https://relay-eu.example", "https://relay-us.example"];

fn announcing(c: &mut onedevice_server::Config) {
    c.relays = ANNOUNCED.iter().map(|r| r.to_string()).collect();
}

/// A GUI-shaped component that can log out.
async fn manager(core: &TestCore) -> TestComponent {
    spawn_component(core, "gui", "gui", &["session.manage", "session.read"]).await
}

/// The happy path: a session against an announcing server persists the list
/// and hands it to the transport, once (an unchanged announcement is not
/// re-pushed); a restart with the server CUT re-announces from the disk
/// cache, which is the availability half of the design (the fleet keeps its
/// operator's relays with the server down); and the logout forgets it all.
#[tokio::test(flavor = "multi_thread")]
async fn the_announcement_is_learned_cached_rebound_and_forgotten() {
    let server = TestServer::start_with(announcing).await;
    let switchboard = MemorySwitchboard::new();
    let core = TestCore::start_enrolled_on(&server, &switchboard).await;
    let node_id = core.node_id();
    let mut c = manager(&core).await;
    wait_server_connected(&mut c, true).await;

    // The session establishment fetched the descriptor and applied the list.
    let announced: Vec<String> = ANNOUNCED.iter().map(|r| r.to_string()).collect();
    let expected = vec![announced.clone()];
    eventually(
        async || switchboard.announced(&node_id) == expected,
        "the announcement reaching the transport",
    )
    .await;
    let cache = core.config_dir().join("announced-relays.json");
    assert!(cache.exists(), "the announcement is cached on disk");

    // A reconnection re-reads the descriptor, and the UNCHANGED word is not
    // re-pushed: the transport hears the operator's list exactly once per
    // actual change, however many sessions come and go.
    server.cut();
    wait_server_connected(&mut c, false).await;
    server.restore();
    wait_server_connected(&mut c, true).await;
    assert_eq!(
        switchboard.announced(&node_id).len(),
        1,
        "an unchanged announcement must not be re-pushed on reconnection"
    );

    // A restart with the server cut: the boot announces the CACHED list to
    // the transport (same switchboard, same endpoint), before any session.
    server.cut();
    drop(c);
    let core = core.restart().await;
    eventually(
        async || switchboard.announced(&node_id).len() == 2,
        "the cached announcement re-applied at boot",
    )
    .await;
    assert_eq!(switchboard.announced(&node_id)[1], announced);

    // Logout: the standing word ends with the relationship, on disk AND in
    // the transport: a bind later in this same run (a pairing window, a
    // dial) must not ride the departed operator's relays.
    let mut c = manager(&core).await;
    c.request("session.logout", json!({}))
        .await
        .expect("session.logout");
    assert!(
        !cache.exists(),
        "the announcement does not outlive the session"
    );
    assert_eq!(
        switchboard.announced(&node_id).last(),
        Some(&Vec::new()),
        "the transport must hear the emptying too"
    );
}

/// A server REVOKING the device ends the standing word exactly like a
/// logout: the struck device does not keep riding, or caching, the
/// operator's relays.
#[tokio::test(flavor = "multi_thread")]
async fn a_revocation_forgets_the_announcement() {
    let server = TestServer::start_with(announcing).await;
    let mut admin = server.online_device("PC-Admin", "macos").await;
    let switchboard = MemorySwitchboard::new();
    let core = TestCore::start_enrolled_on(&server, &switchboard).await;
    let node_id = core.node_id();
    let mut c = manager(&core).await;
    wait_server_connected(&mut c, true).await;
    let cache = core.config_dir().join("announced-relays.json");
    eventually(
        async || cache.exists(),
        "the announcement reaching the disk",
    )
    .await;

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
    assert!(!cache.exists(), "a struck device keeps no announcement");
    assert_eq!(
        switchboard.announced(&node_id).last(),
        Some(&Vec::new()),
        "the transport must hear the emptying too"
    );
}

/// A server that announces nothing leaves no trace: the empty word matches
/// the empty cache, so nothing is stored and the transport hears nothing —
/// an off-default fleet stays exactly as quiet as before #105.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_announcing_nothing_changes_nothing() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let core = TestCore::start_enrolled_on(&server, &switchboard).await;
    let node_id = core.node_id();
    let mut c = manager(&core).await;
    wait_server_connected(&mut c, true).await;

    assert!(
        switchboard.announced(&node_id).is_empty(),
        "no announcement, no push"
    );
    assert!(
        !core.config_dir().join("announced-relays.json").exists(),
        "nothing to cache"
    );
}

/// A WITHDRAWN announcement is a word too. The cache carries relays from a
/// previous session (seeded on disk here, the state a fleet is in after its
/// operator removes the relays); the boot announces that cached list, and
/// the next session, hearing the server now announce NONE, must replace the
/// cache with the empty word and tell the transport — or the old relays
/// would come back from the cache at every boot, forever.
#[tokio::test(flavor = "multi_thread")]
async fn a_withdrawn_announcement_replaces_the_cached_one() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let core = TestCore::start_enrolled_on(&server, &switchboard).await;
    let node_id = core.node_id();
    let mut c = manager(&core).await;
    wait_server_connected(&mut c, true).await;
    assert!(switchboard.announced(&node_id).is_empty());

    // Yesterday's word, as a previous session against an announcing server
    // left it (same shape `relays::store` writes).
    drop(c);
    std::fs::write(
        core.config_dir().join("announced-relays.json"),
        json!({ "relays": ["https://relay-old.example"] }).to_string(),
    )
    .expect("seed the cache");

    let core = core.restart().await;
    // Boot: the cached (stale) list reaches the transport first...
    eventually(
        async || switchboard.announced(&node_id).first().map(Vec::len) == Some(1),
        "the seeded cache re-applied at boot",
    )
    .await;
    // ...then the session hears the withdrawal: the empty word, pushed and
    // persisted (an unchanged-vs-cache fetch would push nothing).
    let mut c = manager(&core).await;
    wait_server_connected(&mut c, true).await;
    eventually(
        async || switchboard.announced(&node_id).len() == 2,
        "the withdrawal reaching the transport",
    )
    .await;
    assert_eq!(switchboard.announced(&node_id)[1], Vec::<String>::new());
    let cache = std::fs::read_to_string(core.config_dir().join("announced-relays.json"))
        .expect("the empty word is persisted");
    assert!(cache.contains("[]"), "{cache}");
}
