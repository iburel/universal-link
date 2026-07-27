// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! What happens when a menu entry is clicked: the courier's request, and
//! everything the manager refuses.

use serde_json::json;
use universallink_menu::channel::{Response, error};

use crate::support::*;

/// The whole click path, and it must be pinned END TO END: the device the user
/// clicked and the files they selected are what the Core is asked to send. Two
/// peers are online and the SECOND is clicked on purpose — sending to "the first
/// target" instead of the clicked one is a mistake no assertion on the shape of
/// the reply could ever catch, and it would silently deliver the user's files to
/// the wrong machine.
#[tokio::test]
async fn a_click_sends_exactly_the_selected_files_to_exactly_the_clicked_device() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let mut watcher = TransferWatcher::connect(&core).await;
    let manager = Manager::start(&core).await;

    // Named so the menu order is PC-A then PC-B (it sorts by label, not by id).
    let first = server.attested_peer(&code, "PC-A", "linux").await;
    let second = server.attested_peer(&code, "PC-B", "linux").await;
    manager
        .await_targets(&[&first.device_id, &second.device_id])
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let paths = vec![a_file(dir.path(), "notes.txt"), a_file(dir.path(), "b.txt")];

    let transfer_id = match manager.click(&second.device_id, &paths).await {
        Response::Accepted { transfer_id } => {
            assert!(
                transfer_id.starts_with("t_"),
                "unexpected transfer id: {transfer_id}"
            );
            transfer_id
        }
        other => panic!("expected an accepted send, got {other:?}"),
    };

    // What the Core actually took on. The bytes themselves are T2's suite: the
    // harness peer is not a real data-plane endpoint, so the transfer fails right
    // after this — which is why the assertion is on the manifest, not on arrival.
    let started = watcher.started().await;
    assert_eq!(
        started["transfer_id"].as_str(),
        Some(transfer_id.as_str()),
        "the courier's transfer must be the one the Core started"
    );
    assert_eq!(
        started["device_id"].as_str(),
        Some(second.device_id.as_str()),
        "the files went to the wrong device"
    );
    assert_eq!(
        TransferWatcher::manifest_names(&started),
        ["notes.txt", "b.txt"],
        "the manifest must be exactly the selection"
    );
}

/// Decision 3's accepted residual, and the only place the Core's refusal is
/// relayed: a device attested under ANOTHER account key is offered (we cannot
/// verify a signature without the account key, and a component must not hold one)
/// and its click fails at the Core. The courier must learn the real reason, not a
/// fabricated success.
#[tokio::test]
async fn a_refusal_from_the_core_is_relayed_verbatim() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    // Logged in, with OUR account key installed — that is what makes the peer's
    // foreign attestation a mismatch rather than an absence.
    let _our_code = login(&core).await;
    let manager = Manager::start(&core).await;

    // Attested, but under a key that is not this account's.
    let foreign_code = universallink_core::account_key::generate_recovery_code();
    let rogue = server
        .attested_peer(&foreign_code, "PC-Rogue", "linux")
        .await;
    manager.await_targets(&[&rogue.device_id]).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let paths = vec![a_file(dir.path(), "notes.txt")];

    assert_eq!(
        manager.click(&rogue.device_id, &paths).await,
        Response::Failed {
            // Fail-closed and indistinguishable from "absent", by design.
            error: "DEVICE_UNKNOWN".into()
        }
    );
}

/// Fail-closed, and locally. A stale artifact is the normal case, not an
/// exception: family-A entries live on disk, so one written before a peer went
/// offline — or left behind by a manager that crashed — will be clicked. It must
/// not reach the Core.
#[tokio::test]
async fn a_click_on_a_device_that_is_not_a_target_is_refused_without_asking_the_core() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    manager.await_targets(&[&peer.device_id]).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let paths = vec![a_file(dir.path(), "notes.txt")];

    // A device that never existed.
    assert_eq!(
        manager.click("d_nobody", &paths).await,
        Response::failed(error::NO_SUCH_TARGET)
    );

    // And one that existed a moment ago: the very case a stale entry produces.
    let gone = peer.device_id.clone();
    drop(peer);
    manager.await_targets(&[]).await;
    assert_eq!(
        manager.click(&gone, &paths).await,
        Response::failed(error::NO_SUCH_TARGET)
    );
}

#[tokio::test]
async fn the_targets_pull_answers_what_the_surface_shows() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    // Before anything is online: an empty list is an answer, not an error.
    assert_eq!(manager.ask_targets().await, Response::Targets(vec![]));

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    manager.await_targets(&[&peer.device_id]).await;

    match manager.ask_targets().await {
        Response::Targets(targets) => {
            assert_eq!(targets, manager.renders.current());
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0].name, "PC-B");
        }
        other => panic!("expected a target list, got {other:?}"),
    }
}

/// The Core resolves a relative path against ITS working directory, which is not
/// the file manager's — so the same name would mean a different file, or none.
/// Refused rather than guessed. (The helper makes paths absolute before sending;
/// this is the manager's own guard, reached with a raw line.)
#[tokio::test]
async fn a_relative_path_is_refused() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let code = login(&core).await;
    let manager = Manager::start(&core).await;

    let peer = server.attested_peer(&code, "PC-B", "linux").await;
    manager.await_targets(&[&peer.device_id]).await;

    let line = json!({
        "v": 1,
        "kind": "send",
        "device_id": peer.device_id,
        "paths": ["notes.txt"],
    })
    .to_string()
        + "\n";
    let reply = raw_exchange(manager.channel_path(), &line).await;
    assert_eq!(
        Response::parse(&reply).expect("reply"),
        Response::failed(error::RELATIVE_PATH)
    );
}

/// A courier from a future version must be refused, not misread: guessing what a
/// protocol we do not know meant would mean sending files on a guess.
#[tokio::test]
async fn a_foreign_protocol_version_is_refused() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let manager = Manager::start(&core).await;

    let line = json!({ "v": 99, "kind": "targets" }).to_string() + "\n";
    let reply = raw_exchange(manager.channel_path(), &line).await;
    assert_eq!(
        Response::parse(&reply).expect("reply"),
        Response::failed(error::UNSUPPORTED_VERSION)
    );
}

#[tokio::test]
async fn a_malformed_request_gets_a_refusal_not_a_hang_up() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let manager = Manager::start(&core).await;

    let reply = raw_exchange(manager.channel_path(), "this is not json\n").await;
    assert_eq!(
        Response::parse(&reply).expect("reply"),
        Response::failed(error::BAD_REQUEST)
    );

    // The channel keeps serving afterwards: one bad courier is not a poison.
    assert_eq!(manager.ask_targets().await, Response::Targets(vec![]));
}

/// Nothing is allocated on a length the peer merely claims: the read is bounded,
/// so an overrun is cut short instead of growing a buffer. The manager refuses and
/// hangs up while the courier is still writing — so the courier may well see a
/// broken pipe rather than our reply, and that is the intended fail-fast. What
/// must hold either way: the manager survives and keeps serving.
#[tokio::test]
async fn an_oversized_request_cannot_grow_the_manager() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let manager = Manager::start(&core).await;

    let filler = "A".repeat(17 * 1024 * 1024);
    let line = json!({ "v": 1, "kind": "targets", "pad": filler }).to_string() + "\n";
    if let Some(reply) = try_raw_exchange(manager.channel_path(), &line).await {
        assert_eq!(
            Response::parse(&reply).expect("reply"),
            Response::failed(error::REQUEST_TOO_LARGE)
        );
    }

    assert_eq!(manager.ask_targets().await, Response::Targets(vec![]));
}

/// Exclusivity: two managers would fight over the same artifacts, each undoing
/// the other's rewrite. The second one must recognize the situation and stand
/// down, not fail.
#[tokio::test]
async fn a_second_manager_cannot_take_the_channel() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let manager = Manager::start(&core).await;

    match universallink_menu::channel::bind(manager.channel_path()) {
        Err(universallink_menu::channel::BindError::AlreadyRunning) => {}
        Ok(_) => panic!("two managers must not hold the same channel"),
        Err(e) => panic!("expected AlreadyRunning, got {e}"),
    }
}

/// The channel is a private surface: on unix the socket is owner-only, and the
/// peer's uid is checked on top (macOS does not honor a socket file's mode).
#[cfg(unix)]
#[tokio::test]
async fn the_channel_socket_is_not_readable_by_others() {
    use std::os::unix::fs::PermissionsExt;

    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let manager = Manager::start(&core).await;

    let mode = std::fs::metadata(manager.channel_path())
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "unexpected socket mode: {mode:o}");
}

/// A graceful stop leaves nothing behind for a courier to connect to — the
/// socket and the exclusivity lock go with the manager.
#[cfg(unix)]
#[tokio::test]
async fn stopping_removes_the_socket() {
    let server = TestServer::start().await;
    let core = TestCore::start(&server).await;
    let manager = Manager::start(&core).await;
    let path = manager.channel_path().to_path_buf();

    manager.stop().await;
    assert!(!path.exists(), "the socket outlived the manager");
}
