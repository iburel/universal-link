// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! What the menu offers, and when it stops offering it.

use universallink_ipc_client::{ClientConfig, TokenSource};
use universallink_menu::Outcome;

use crate::support::*;

/// The manager's identity has to be one the Core actually accepts. This matters
/// more than it looks: a refused hello produces NO event at all — the client just
/// backs off and retries forever — so a wrong role, scope or topic would show up
/// as a component that silently never works, not as an error.
#[tokio::test]
async fn the_core_accepts_the_role_and_scopes_the_manager_asks_for() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;

    let (_client, mut events) = universallink_ipc_client::spawn(ClientConfig {
        ipc_path: core.ipc_path(),
        token: TokenSource::Spawn(core.mint(ROLE, SCOPES)),
        name: "universallink-menu".into(),
        version: "0".into(),
        role: ROLE.into(),
        scopes: SCOPES.iter().map(|s| (*s).to_string()).collect(),
        topics: TOPICS.iter().map(|t| (*t).to_string()).collect(),
        served_methods: vec![],
        reconnect_base_delay: std::time::Duration::from_millis(50),
        request_timeout: RESPONSE_TIMEOUT,
    });

    let granted = expect_connected(&mut events).await;
    assert_eq!(
        granted, SCOPES,
        "the supervisor's grant must cover exactly this"
    );
}

#[tokio::test]
async fn an_online_attested_peer_becomes_a_menu_entry() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    manager.await_targets(&[&peer.device_id]).await;
    assert_eq!(manager.renders.current_names(), ["PC-B"]);

    // And it stays put: no rewrite of the surface without something to show.
    manager.assert_stable(&[&peer.device_id]).await;
}

/// The Core is the only holder of the account key, so the manager checks that a
/// device carries *an* attestation, not that it is valid. A device that has never
/// published one cannot be sent to at all (`DEVICE_UNKNOWN`), so offering it
/// would be offering a click that fails.
#[tokio::test]
async fn a_peer_that_never_attested_is_not_offered() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    let bare = server.unattested_peer("PC-Bare", "linux").await;
    let good = server.attested_peer(&code, "PC-Good", "linux").await;

    manager.await_targets(&[&good.device_id]).await;
    assert!(
        !manager.renders.current_ids().contains(&bare.device_id),
        "an unattested device must not be a target"
    );
}

/// Decision of 2026-07-27: the phone shares but does not receive — a file sent to
/// it lands in app-private storage nothing opens yet. Offering it would lose files.
#[tokio::test]
async fn the_phone_is_not_offered_even_when_online_and_attested() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    let phone = server.attested_peer(&code, "OnePlus", "android").await;
    let pc = server.attested_peer(&code, "PC-B", "linux").await;

    manager.await_targets(&[&pc.device_id]).await;
    assert!(
        !manager.renders.current_ids().contains(&phone.device_id),
        "the phone must not be a target"
    );
}

/// We are not a destination for ourselves. The Core is in its own directory
/// snapshot, online and attested — only `is_self` keeps it out.
#[tokio::test]
async fn the_core_itself_is_never_a_target() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    manager.await_targets(&[&peer.device_id]).await;
    assert!(
        !manager
            .renders
            .current_names()
            .contains(&CORE_DEVICE_NAME.to_string()),
        "the local device must not be a target"
    );
}

#[tokio::test]
async fn a_peer_going_offline_loses_its_entry() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    manager.await_targets(&[&peer.device_id]).await;

    // Its connection IS its presence: dropping it makes the server broadcast
    // `device.offline`.
    drop(peer);
    manager.await_targets(&[]).await;
}

#[tokio::test]
async fn a_rename_relabels_the_entry() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    let mut peer = server.attested_peer(&code, "PC-B", "linux").await;
    manager.await_targets(&[&peer.device_id]).await;

    // Renamed from the peer's own connection: the Core learns it as
    // `device.updated`, which carries a whole record.
    peer.conn
        .request(
            "devices.rename",
            serde_json::json!({ "device_id": peer.device_id, "name": "Studio" }),
        )
        .await
        .expect("devices.rename");

    let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
    while manager.renders.current_names() != ["Studio"] {
        assert!(
            tokio::time::Instant::now() < deadline,
            "still showing {:?}",
            manager.renders.current_names()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // The entry is the same device throughout — a rename is not a new target.
    assert_eq!(manager.renders.current_ids(), [peer.device_id]);
}

/// Fail-closed: no session, no menu. The Core here is configured but never
/// logged in, so `devices.list` refuses outright — and the manager must read that
/// as "nothing to offer".
///
/// `assert_stable` also pins the other half: the surface is written ONCE and then
/// left alone. Every snapshot in this state recomputes the same empty list, and
/// rewriting registry keys or `.desktop` files to show the same nothing would be
/// pure churn on the user's desktop.
#[tokio::test]
async fn a_signed_out_core_offers_nothing() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let manager = Manager::start(&core).await;

    manager.assert_stable(&[]).await;
}

/// A logout empties the menu, even though the Core keeps serving its directory
/// cache: the `online` flags in it are last-known, not current.
#[tokio::test]
async fn logging_out_empties_the_menu() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    manager.await_targets(&[&peer.device_id]).await;

    let (gui, mut events) = universallink_ipc_client::spawn(ClientConfig {
        ipc_path: core.ipc_path(),
        token: TokenSource::Spawn(core.mint("gui", &["session.read", "session.manage"])),
        name: "harness-gui".into(),
        version: "0".into(),
        role: "gui".into(),
        scopes: vec!["session.read".into(), "session.manage".into()],
        topics: vec!["session".into()],
        served_methods: vec![],
        reconnect_base_delay: std::time::Duration::from_millis(50),
        request_timeout: RESPONSE_TIMEOUT,
    });
    expect_connected(&mut events).await;
    gui.request("session.logout", serde_json::json!({}))
        .await
        .expect("session.logout");

    manager.await_targets(&[]).await;
}

/// The graceful stop the supervisor uses on all three OSes — and, on Windows, the
/// only one it has. It is therefore the only chance to take the entries down.
#[tokio::test]
async fn standard_input_closing_stops_the_manager_and_clears_the_menu() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    manager.await_targets(&[&peer.device_id]).await;

    let renders = manager.renders.clone();
    assert_eq!(manager.stop().await, Outcome::StdinClosed);
    assert!(
        renders.current().is_empty(),
        "no manager, no entry: {:?}",
        renders.current()
    );
}

/// The spawn token is single-use, so a lost connection must end the process: the
/// supervisor restarts it with a fresh one. And the entries go away with it —
/// otherwise a click would reach a manager that no longer exists.
#[tokio::test]
async fn losing_the_core_ends_the_manager_and_clears_the_menu() {
    let server = TestServer::start().await;
    let mut core = TestCore::start(&server).await;
    let code = login(&core).await;
    let mut manager = Manager::start(&core).await;

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    manager.await_targets(&[&peer.device_id]).await;

    core.stop();

    let renders = manager.renders.clone();
    assert_eq!(manager.wait().await, Outcome::ConnectionLost);
    assert!(
        renders.current().is_empty(),
        "the entries must not outlive the connection: {:?}",
        renders.current()
    );
}
