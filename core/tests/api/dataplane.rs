// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Data plane (T2): two Cores of the same account reach each other via the
//! directory (`node_id` + `relay_url`) and transfer files to each other — here
//! a memory transport (the real iroh impl is tested natively on the daemon
//! side). We exercise the PROD protocol (`files.send` → offer + body → ack),
//! tracking via the `transfers` topic, writing to disk, cancellation, collision
//! resolution, and above all the C7 REFUSALS (absent/foreign attestation, no
//! account key) on this same production path.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use universallink_core::{IoStream, OutgoingFile, PeerAddr, PeerTransport, send_transfer};
use universallink_test_support::memory_transport::{MemorySwitchboard, MemoryTransport};

use crate::support::*;

#[tokio::test(flavor = "multi_thread")]
async fn two_cores_transfer_a_file_through_the_directory() {
    let server = TestServer::start().await;
    let (a, b) = TestCore::start_pair(&server).await;

    let mut sender = spawn_component(
        &a,
        "sender",
        "menu-backend",
        &[
            "files.send",
            "devices.read",
            "session.read",
            "transfers.read",
        ],
    )
    .await;
    let mut watcher = spawn_component(
        &b,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut sender).await;
    subscribe_transfers(&mut watcher).await;
    wait_server_connected(&mut sender, true).await;
    wait_server_connected(&mut watcher, true).await;
    wait_reachable(&mut sender, b.device_id()).await;
    // Reverse barrier: the receiver must have attested the sender too, or it
    // refuses the sender's first stream fail-closed and SILENTLY (C7) — the
    // sender only sees a broken pipe and this watcher would wait forever.
    // `wait_reachable` above proves only A→B; a transfer also needs B→A.
    wait_attested(&mut watcher, a.device_id()).await;

    let contents = b"hello from A";
    let src = a.write_source("hello.txt", contents);
    let r = sender
        .request(
            "files.send",
            json!({ "device_id": b.device_id(), "paths": [src.to_str().unwrap()] }),
        )
        .await
        .expect("files.send");
    let transfer_id = r["transfer_id"].as_str().expect("transfer_id").to_string();

    // The receiver sees the inbound one: the sender, the manifest.
    let incoming = watcher.wait_notification("transfer.incoming").await;
    assert_eq!(incoming["device_id"], json!(a.device_id()));
    assert_eq!(incoming["files"][0]["name"], json!("hello.txt"));
    assert_eq!(incoming["files"][0]["size"], json!(contents.len()));

    // Then the end, with the path actually written in ITS receive directory.
    let finished = watcher.wait_notification("transfer.finished").await;
    let written = finished["paths"][0].as_str().expect("written path");
    assert_eq!(std::fs::read(written).expect("received file"), contents);
    assert!(
        Path::new(written).starts_with(b.receive_dir()),
        "written in the receive directory: {written}"
    );

    // The sender sees its cycle: started (with the announced total), at least
    // one progress (including the final point done==total), then finished — its
    // own transfer_id.
    let started = sender.wait_notification("transfer.started").await;
    assert_eq!(started["transfer_id"], json!(transfer_id));
    assert_eq!(started["total"], json!(contents.len()));
    let mut saw_progress = false;
    let mut saw_full = false;
    let sent = loop {
        let (method, params) = sender.notification().await;
        match method.as_str() {
            "transfer.progress" => {
                let done = params["done"].as_u64().expect("done");
                let total = params["total"].as_u64().expect("total");
                assert!(done <= total, "consistent progress: {done}/{total}");
                saw_progress = true;
                saw_full |= done == total;
            }
            "transfer.finished" => break params,
            other => panic!("unexpected notification on the sender side: {other}"),
        }
    };
    assert!(saw_progress, "at least one transfer.progress");
    assert!(saw_full, "a final progress point (done == total)");
    assert_eq!(sent["transfer_id"], json!(transfer_id));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transfer_of_several_files_lands_intact() {
    let server = TestServer::start().await;
    let (a, b) = TestCore::start_pair(&server).await;

    let mut sender = spawn_component(
        &a,
        "sender",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    let mut watcher = spawn_component(
        &b,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut watcher).await;
    wait_server_connected(&mut sender, true).await;
    wait_server_connected(&mut watcher, true).await;
    wait_reachable(&mut sender, b.device_id()).await;
    // Reverse barrier (see `two_cores_transfer_a_file_through_the_directory`):
    // the receiver must attest the sender too, else its first stream is refused
    // fail-closed and silently.
    wait_attested(&mut watcher, a.device_id()).await;

    // Three files, including an EMPTY one (0 bytes) and one bigger than a chunk
    // (streaming boundary): the receiver reads exactly `size` bytes per file.
    let one = b"first".to_vec();
    let empty = Vec::new();
    let three = vec![9u8; 100_000];
    let srcs = [
        a.write_source("one.txt", &one),
        a.write_source("empty.dat", &empty),
        a.write_source("three.bin", &three),
    ];
    let paths: Vec<&str> = srcs.iter().map(|p| p.to_str().unwrap()).collect();
    sender
        .request(
            "files.send",
            json!({ "device_id": b.device_id(), "paths": paths }),
        )
        .await
        .expect("files.send");

    let incoming = watcher.wait_notification("transfer.incoming").await;
    assert_eq!(incoming["files"].as_array().expect("files").len(), 3);
    let finished = watcher.wait_notification("transfer.finished").await;
    let written = finished["paths"].as_array().expect("paths");
    assert_eq!(written.len(), 3);
    for (path, expected) in written.iter().zip([&one, &empty, &three]) {
        let path = path.as_str().expect("path");
        assert_eq!(
            &std::fs::read(path).expect("received file"),
            expected,
            "contents of {path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_file_with_the_same_name_does_not_overwrite() {
    let server = TestServer::start().await;
    let (a, b) = TestCore::start_pair(&server).await;

    let mut sender = spawn_component(
        &a,
        "sender",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    let mut watcher = spawn_component(
        &b,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut watcher).await;
    wait_server_connected(&mut sender, true).await;
    wait_server_connected(&mut watcher, true).await;
    wait_reachable(&mut sender, b.device_id()).await;
    // Reverse barrier (see `two_cores_transfer_a_file_through_the_directory`):
    // the receiver must attest the sender too, else its first stream is refused
    // fail-closed and silently.
    wait_attested(&mut watcher, a.device_id()).await;

    // First "doc.txt".
    let src = a.write_source("doc.txt", b"content A");
    send_one(&mut sender, b.device_id(), &src).await;
    let first = watcher.wait_notification("transfer.finished").await;
    let first_path = first["paths"][0].as_str().expect("path").to_string();
    assert_eq!(std::fs::read(&first_path).unwrap(), b"content A");

    // Second "doc.txt" (the first transfer is done: rewriting the source has no
    // effect on it): it must NOT overwrite — "(n)" suffix.
    let src = a.write_source("doc.txt", b"content B");
    send_one(&mut sender, b.device_id(), &src).await;
    let second = watcher.wait_notification("transfer.finished").await;
    let second_path = second["paths"][0].as_str().expect("path").to_string();
    assert_ne!(
        second_path, first_path,
        "the second must not reuse the name"
    );
    assert_eq!(std::fs::read(&second_path).unwrap(), b"content B");
    // And the first is intact.
    assert_eq!(std::fs::read(&first_path).unwrap(), b"content A");
    assert_eq!(
        std::fs::read(b.receive_dir().join("doc.txt")).unwrap(),
        b"content A"
    );
    assert_eq!(
        std::fs::read(b.receive_dir().join("doc (1).txt")).unwrap(),
        b"content B"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_tree_is_walked_and_recreated_on_the_receiver() {
    let server = TestServer::start().await;
    let (a, b) = TestCore::start_pair(&server).await;

    let mut sender = spawn_component(
        &a,
        "sender",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    let mut watcher = spawn_component(
        &b,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut watcher).await;
    wait_server_connected(&mut sender, true).await;
    wait_server_connected(&mut watcher, true).await;
    wait_reachable(&mut sender, b.device_id()).await;
    wait_attested(&mut watcher, a.device_id()).await;

    // top/a.txt, top/sub/b.bin (nested), top/empty/ (empty dir, no body).
    let (_guard, top) = scratch_tree();
    sender
        .request(
            "files.send",
            json!({ "device_id": b.device_id(), "paths": [top.to_str().unwrap()] }),
        )
        .await
        .expect("files.send a folder");

    // The manifest carries the empty directory as a `dir:true`, sizeless entry.
    let incoming = watcher.wait_notification("transfer.incoming").await;
    let files = incoming["files"].as_array().expect("files");
    assert_eq!(files.len(), 3);
    assert!(
        files
            .iter()
            .any(|f| f["dir"] == json!(true) && f["size"] == json!(0)),
        "the empty folder is announced as a dir: {files:?}"
    );

    watcher.wait_notification("transfer.finished").await;

    // The tree is recreated verbatim under the receive directory.
    let root = b.receive_dir().join("top");
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"alpha");
    assert_eq!(
        std::fs::read(root.join("sub").join("b.bin")).unwrap(),
        b"beta bytes"
    );
    assert!(
        root.join("empty").is_dir(),
        "the empty directory was recreated"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_folder_of_the_same_name_lands_beside_the_first() {
    let server = TestServer::start().await;
    let (a, b) = TestCore::start_pair(&server).await;

    let mut sender = spawn_component(
        &a,
        "sender",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    let mut watcher = spawn_component(
        &b,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut watcher).await;
    wait_server_connected(&mut sender, true).await;
    wait_server_connected(&mut watcher, true).await;
    wait_reachable(&mut sender, b.device_id()).await;
    wait_attested(&mut watcher, a.device_id()).await;

    let (_guard, top) = scratch_tree();
    let path = top.to_str().unwrap();

    // First copy: lands in `top/`.
    sender
        .request(
            "files.send",
            json!({ "device_id": b.device_id(), "paths": [path] }),
        )
        .await
        .expect("files.send #1");
    watcher.wait_notification("transfer.finished").await;

    // Second copy of the SAME folder: the top-level directory already exists, so
    // the whole subtree is redirected to a fresh sibling — never merged into, or
    // clobbering, the first.
    sender
        .request(
            "files.send",
            json!({ "device_id": b.device_id(), "paths": [path] }),
        )
        .await
        .expect("files.send #2");
    watcher.wait_notification("transfer.finished").await;

    assert_eq!(
        std::fs::read(b.receive_dir().join("top").join("a.txt")).unwrap(),
        b"alpha",
        "the first copy is intact"
    );
    assert_eq!(
        std::fs::read(b.receive_dir().join("top (1)").join("a.txt")).unwrap(),
        b"alpha",
        "the second copy lands in a fresh sibling"
    );
    assert!(b.receive_dir().join("top (1)").join("empty").is_dir());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_folder_path_that_escapes_is_refused_and_writes_nothing() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let victim = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let (rogue_id, _conn, rogue) = attested_sink(&server, &switchboard, &code).await;

    let mut vc = spawn_component(&victim, "obs-v", "tray", &["session.read", "devices.read"]).await;
    wait_server_connected(&mut vc, true).await;
    wait_attested(&mut vc, &rogue_id).await;

    let peer = victim_peer(&victim);
    let mut stream = rogue.open(&peer).await.expect("the switchboard routes");
    // A `/`-separated name climbing out with a `..` segment — the tree receiver
    // accepts `/` (a folder path) but must still refuse a traversal segment.
    let evil = b"escape";
    offer_then_close(
        &mut stream,
        &[("sub/../../pown.txt", evil.len() as u64, evil)],
    )
    .await;

    assert!(received_files(&victim).is_empty(), "no file written");
    let escape = victim
        .receive_dir()
        .parent()
        .expect("parent directory")
        .join("pown.txt");
    assert!(!escape.exists(), "no write outside the receive directory");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_offer_with_a_duplicate_path_is_refused_and_writes_nothing() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let victim = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let (rogue_id, _conn, rogue) = attested_sink(&server, &switchboard, &code).await;

    let mut vc = spawn_component(&victim, "obs-v", "tray", &["session.read", "devices.read"]).await;
    wait_server_connected(&mut vc, true).await;
    wait_attested(&mut vc, &rogue_id).await;

    let peer = victim_peer(&victim);
    let mut stream = rogue.open(&peer).await.expect("the switchboard routes");
    // Two entries with the SAME nested path: a conforming sender never emits one
    // (freeze_manifest's names are unique), so the receiver refuses the whole
    // offer rather than let the second silently clobber the first.
    offer_then_close(
        &mut stream,
        &[("dup/a.txt", 3, b"AAA"), ("dup/a.txt", 3, b"BBB")],
    )
    .await;

    assert!(
        received_files(&victim).is_empty(),
        "a duplicate-path offer writes nothing"
    );
    assert!(
        !victim.receive_dir().join("dup").exists(),
        "not even the top-level directory is created"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flat_offer_with_two_same_basename_files_still_disambiguates() {
    // Version skew: a pre-folder sender did not uniquify basenames, so it could
    // offer two files both named "f.txt" in ONE transfer. The receiver must still
    // disambiguate them (f.txt + f (1).txt) as before — the duplicate-path guard
    // is scoped to directories and nested paths, never a plain top-level file.
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let victim = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let (rogue_id, _conn, rogue) = attested_sink(&server, &switchboard, &code).await;

    let mut watcher = spawn_component(
        &victim,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut watcher).await;
    wait_server_connected(&mut watcher, true).await;
    wait_attested(&mut watcher, &rogue_id).await;

    let peer = victim_peer(&victim);
    let mut stream = rogue.open(&peer).await.expect("the switchboard routes");
    offer_then_close(&mut stream, &[("f.txt", 3, b"AAA"), ("f.txt", 3, b"BBB")]).await;

    let finished = watcher.wait_notification("transfer.finished").await;
    assert_eq!(finished["paths"].as_array().expect("paths").len(), 2);
    assert_eq!(
        std::fs::read(victim.receive_dir().join("f.txt")).unwrap(),
        b"AAA"
    );
    assert_eq!(
        std::fs::read(victim.receive_dir().join("f (1).txt")).unwrap(),
        b"BBB"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_outside_the_directory_is_refused() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let a = TestCore::start_enrolled_on(&server, &switchboard).await;

    let mut ac = spawn_component(&a, "obs-a", "tray", &["session.read"]).await;
    wait_server_connected(&mut ac, true).await;

    // An endpoint that speaks the right protocol and knows A's address, but
    // whose key is NOT a device of the account: A's `serve` loop closes without
    // a byte. iroh authenticates the key; it is the directory that says whether
    // it is one of ours — and that refusal is the one from `peer_in_directory`.
    let rogue = switchboard.endpoint("key-outside-account", None);
    let a_relay = format!("iroh+memory://{}", a.key().node_id());
    transfer_is_refused(&rogue, &a.key().node_id(), &a_relay).await;
    assert!(
        received_files(&a).is_empty(),
        "A writes nothing from a refused peer"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_with_a_mismatched_account_key_is_refused() {
    let server = TestServer::start().await;
    let (a, b) = TestCore::start_mismatched_pair(&server).await;

    let mut sa = spawn_component(
        &a,
        "sa",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    let mut sb = spawn_component(
        &b,
        "sb",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    wait_server_connected(&mut sa, true).await;
    wait_server_connected(&mut sb, true).await;

    // Each one SEES the other in the directory, attestation included — but
    // signed under a foreign account key: the refusal will therefore be about
    // the verification, not about a missing record.
    wait_attested(&mut sa, b.device_id()).await;
    wait_attested(&mut sb, a.device_id()).await;

    // `resolve_peer` verifies the target's attestation under OUR key BEFORE
    // opening: it does not add up → unresolved → DEVICE_UNKNOWN, no byte
    // leaves. Mere directory membership no longer counts as authorization.
    let (_d, src) = scratch_file(b"hi");
    let path = src.to_str().unwrap();
    let err = sa
        .request(
            "files.send",
            json!({ "device_id": b.device_id(), "paths": [path] }),
        )
        .await
        .expect_err("foreign attestation → refusal");
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");
    let err = sb
        .request(
            "files.send",
            json!({ "device_id": a.device_id(), "paths": [path] }),
        )
        .await
        .expect_err("symmetric refusal");
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_injected_node_id_with_a_foreign_attestation_is_refused_inbound() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let victim_code = universallink_core::account_key::generate_recovery_code();
    let foreign_code = universallink_core::account_key::generate_recovery_code();

    // Victim: a legitimate Core with ITS OWN account key.
    let victim =
        TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&victim_code)).await;
    let mut vc = spawn_component(&victim, "obs-v", "tray", &["session.read", "devices.read"]).await;
    wait_server_connected(&mut vc, true).await;

    // The very threat C7 addresses: an injection into the account directory of
    // a device whose node_id bears an attestation signed under a FOREIGN
    // account key (foreign_code) — not ours.
    let intruder_key = DeviceKey::generate();
    let mut ic = server.connect_direct().await;
    let intruder_id = enroll_key(
        &mut ic,
        &server.oidc,
        &intruder_key,
        TEST_SUB,
        "intruder",
        std::env::consts::OS,
    )
    .await;
    authenticate(&mut ic, &intruder_key, &intruder_id).await;
    let foreign_ak = universallink_core::account_key::account_key_from_code(&foreign_code).unwrap();
    let foreign_att = universallink_core::account_key::attest(&foreign_ak, &intruder_key.node_id());
    let intruder_relay = format!("iroh+memory://{}", intruder_key.node_id());
    ic.request(
        "presence.update",
        json!({ "attestation": foreign_att, "relay_url": intruder_relay }),
    )
    .await
    .expect("presence.update from the intruder");

    // The victim must SEE the intruder, (foreign) attestation included.
    eventually(
        async || {
            let list = vc
                .request("devices.list", json!({}))
                .await
                .expect("devices.list");
            find_device(&list, &intruder_id)
                .get("attestation")
                .and_then(Value::as_str)
                == Some(foreign_att.as_str())
        },
        "intruder's foreign attestation visible to the victim",
    )
    .await;

    // The intruder opens a raw INBOUND stream and attempts a transfer,
    // short-circuiting all of resolve_peer: it is the victim's
    // `peer_in_directory`, alone, that must decide. The node_id IS in the
    // directory, but its attestation does not verify under the victim's key →
    // closed without a byte, nothing written.
    let rogue = switchboard.endpoint(intruder_key.node_id(), Some(intruder_relay.clone()));
    let victim_relay = format!("iroh+memory://{}", victim.key().node_id());
    transfer_is_refused(&rogue, &victim.key().node_id(), &victim_relay).await;
    assert!(
        received_files(&victim).is_empty(),
        "injection: nothing written at the victim"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_core_without_an_account_key_reaches_and_serves_no_one() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    // A: attested. B: enrolled but NEVER joined — no account key.
    let a = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let b = TestCore::start_enrolled_on_with_code(&server, &switchboard, None).await;
    let mut sb = spawn_component(
        &b,
        "sb",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    wait_server_connected(&mut sb, true).await;
    // B sees A (attestation), so that the refusal is about the ABSENCE OF A
    // ROOT and not about a missing record.
    wait_attested(&mut sb, a.device_id()).await;

    // Outbound: without a trust root, resolve_peer short-circuits on
    // account_root=None → DEVICE_UNKNOWN, before any opening.
    let (_d, src) = scratch_file(b"hi");
    let err = sb
        .request(
            "files.send",
            json!({ "device_id": a.device_id(), "paths": [src.to_str().unwrap()] }),
        )
        .await
        .expect_err("without a root, no peer resolved");
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");

    // Inbound: a stream to B is closed — peer_in_directory also short-circuits
    // on account_root=None: without a root, B authorizes no one.
    let rogue = switchboard.endpoint("some-key", None);
    let b_relay = format!("iroh+memory://{}", b.key().node_id());
    transfer_is_refused(&rogue, &b.key().node_id(), &b_relay).await;
    assert!(
        received_files(&b).is_empty(),
        "a Core without a key serves no one"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crafted_traversal_name_is_refused_and_writes_nothing() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let victim = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    // A peer OF THE ACCOUNT (attested under the SAME code) — thus accepted by
    // peer_in_directory — but that forges an offer with a malicious name. The
    // refusal therefore cannot come from C7: it comes from name sanitization.
    let (rogue_id, _conn, rogue) = attested_sink(&server, &switchboard, &code).await;

    let mut vc = spawn_component(&victim, "obs-v", "tray", &["session.read", "devices.read"]).await;
    wait_server_connected(&mut vc, true).await;
    wait_attested(&mut vc, &rogue_id).await;

    let peer = PeerAddr {
        node_id: victim.key().node_id(),
        relay_url: Some(format!("iroh+memory://{}", victim.key().node_id())),
    };
    let mut stream = rogue.open(&peer).await.expect("the switchboard routes");
    // A name that tries to climb up one level (Windows separator not split
    // under Linux): the victim flatly refuses it and writes NOWHERE.
    let evil = b"malicious payload";
    offer_then_close(&mut stream, &[(r"..\..\pown.txt", evil.len() as u64, evil)]).await;

    assert!(received_files(&victim).is_empty(), "no file written");
    let escape = victim
        .receive_dir()
        .parent()
        .expect("parent directory")
        .join("pown.txt");
    assert!(!escape.exists(), "no write outside the receive directory");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_shorter_than_announced_fails_and_writes_nothing() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let victim = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let (rogue_id, _conn, rogue) = attested_sink(&server, &switchboard, &code).await;

    let mut watcher = spawn_component(
        &victim,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut watcher).await;
    wait_server_connected(&mut watcher, true).await;
    wait_attested(&mut watcher, &rogue_id).await;

    let peer = victim_peer(&victim);
    let mut stream = rogue.open(&peer).await.expect("the switchboard routes");
    // Offers 100 bytes, sends only 5 then closes: the receiver sees the EOF
    // before the announced end -> failure, never a silently truncated file.
    offer_then_close(&mut stream, &[("truncated.bin", 100, b"short")]).await;

    let failed = watcher.wait_notification("transfer.failed").await;
    assert!(failed["transfer_id"].is_string());
    assert!(
        received_files(&victim).is_empty(),
        "nothing truncated written"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_file_with_a_bad_name_aborts_the_whole_transfer() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let victim = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let (rogue_id, _conn, rogue) = attested_sink(&server, &switchboard, &code).await;

    let mut watcher = spawn_component(
        &victim,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut watcher).await;
    wait_server_connected(&mut watcher, true).await;
    wait_attested(&mut watcher, &rogue_id).await;

    let peer = victim_peer(&victim);
    let mut stream = rogue.open(&peer).await.expect("the switchboard routes");
    // The 1st file is valid and fully received; the 2nd carries a malicious
    // name. The validation being all-or-nothing (renaming only at the end), the
    // 1st must NOT survive either.
    offer_then_close(
        &mut stream,
        &[("good.txt", 4, b"good"), (r"..\bad", 4, b"evil")],
    )
    .await;

    let failed = watcher.wait_notification("transfer.failed").await;
    assert!(failed["transfer_id"].is_string());
    assert!(
        received_files(&victim).is_empty(),
        "all-or-nothing: the valid file preceding a refused name is not kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_an_inbound_transfer_stops_it() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let victim = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let (rogue_id, _conn, rogue) = attested_sink(&server, &switchboard, &code).await;

    let mut watcher = spawn_component(
        &victim,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    let mut canceller =
        spawn_component(&victim, "canceller", "menu-backend", &["files.send"]).await;
    subscribe_transfers(&mut watcher).await;
    wait_server_connected(&mut watcher, true).await;
    wait_attested(&mut watcher, &rogue_id).await;

    // The intruder offers a big file but sends only a beginning of it, then
    // holds the stream: the victim's reception stays blocked, in flight.
    let peer = victim_peer(&victim);
    let mut held = rogue.open(&peer).await.expect("the switchboard routes");
    offer_then_hold(&mut held, "big.bin", 1_000_000, b"start").await;

    let incoming = watcher.wait_notification("transfer.incoming").await;
    let inbound = incoming["transfer_id"]
        .as_str()
        .expect("transfer_id")
        .to_string();

    canceller
        .request("files.cancel", json!({ "transfer_id": inbound }))
        .await
        .expect("inbound files.cancel");
    let failed = watcher.wait_notification("transfer.failed").await;
    assert_eq!(failed["transfer_id"], json!(inbound));
    assert_eq!(failed["error"], json!("cancelled"));
    let left = received_files(&victim);
    assert!(
        left.is_empty(),
        "cancellation: nothing committed, yet {left:?} remains"
    );
    drop(held);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_attested_peer_without_a_published_relay_is_offline() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let a = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    // A device of the account, attested, but that has NOT published a relay.
    let (offline_id, _conn) = attested_without_relay(&server, &code).await;

    let mut sender = spawn_component(
        &a,
        "sender",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    wait_server_connected(&mut sender, true).await;
    wait_attested(&mut sender, &offline_id).await;

    // Attested (resolved) but without a published relay -> DEVICE_OFFLINE,
    // synchronous.
    let src = a.write_source("x.txt", b"x");
    let err = sender
        .request(
            "files.send",
            json!({ "device_id": offline_id, "paths": [src.to_str().unwrap()] }),
        )
        .await
        .expect_err("attested peer without a relay");
    assert_eq!(err.app_code(), "DEVICE_OFFLINE");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_peer_is_reachable_again_after_a_reconnection() {
    let server = TestServer::start().await;
    let (a, b) = TestCore::start_pair(&server).await;

    let mut sender = spawn_component(
        &a,
        "sender",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    let mut watcher = spawn_component(
        &b,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut watcher).await;
    wait_server_connected(&mut sender, true).await;
    wait_server_connected(&mut watcher, true).await;
    wait_reachable(&mut sender, b.device_id()).await;

    // Outage: the server forgets B's relay along with its connection (it dies
    // with it, doc/server-api.md). The attestation, for its part, SURVIVES the
    // offline period.
    server.cut();
    wait_server_connected(&mut sender, false).await;
    wait_server_connected(&mut watcher, false).await;
    server.restore();
    wait_server_connected(&mut sender, true).await;
    wait_server_connected(&mut watcher, true).await;

    // On reconnection, B must REPUBLISH its relay — a one-shot publication would
    // leave it mute. Once reachable again, A transfers it a file.
    wait_reachable(&mut sender, b.device_id()).await;
    // Reverse barrier (see `two_cores_transfer_a_file_through_the_directory`):
    // B must have attested A too, else it refuses A's stream fail-closed.
    wait_attested(&mut watcher, a.device_id()).await;
    let src = a.write_source("after.txt", b"still here");
    transfer_and_expect(
        &mut sender,
        &mut watcher,
        b.device_id(),
        &src,
        b"still here",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_core_still_transfers_to_its_peer() {
    let server = TestServer::start().await;
    let (a, b) = TestCore::start_pair(&server).await;

    let mut ac = spawn_component(&a, "obs-a", "tray", &["session.read"]).await;
    wait_server_connected(&mut ac, true).await;
    drop(ac);

    // A restarts (new socket, SAME transport: the daemon would rewire iroh with
    // the same device.key). Both directions must stay possible.
    let a = a.restart().await;
    let mut sa = spawn_component(
        &a,
        "sa2",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    let mut wb = spawn_component(
        &b,
        "wb",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut wb).await;
    wait_server_connected(&mut sa, true).await;
    wait_server_connected(&mut wb, true).await;
    wait_reachable(&mut sa, b.device_id()).await;
    // Reverse barrier (see `two_cores_transfer_a_file_through_the_directory`):
    // B must have attested the restarted A too, else it refuses A's stream.
    wait_attested(&mut wb, a.device_id()).await;

    let src = a.write_source("again.txt", b"here again");
    transfer_and_expect(&mut sa, &mut wb, b.device_id(), &src, b"here again").await;

    // And B reaches the restarted A.
    let mut sb = spawn_component(
        &b,
        "sb2",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    let mut wa = spawn_component(
        &a,
        "wa",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    subscribe_transfers(&mut wa).await;
    wait_server_connected(&mut sb, true).await;
    wait_reachable(&mut sb, a.device_id()).await;
    // Reverse barrier: A must have attested B too before B's stream.
    wait_attested(&mut wa, b.device_id()).await;

    let src = b.write_source("youtoo.txt", b"you too");
    transfer_and_expect(&mut sb, &mut wa, a.device_id(), &src, b"you too").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_an_outbound_transfer_stops_it() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let a = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    // A peer of the account (attested under the SAME code) whose transport
    // accepts streams but NEVER reads them: the send blocks, in flight,
    // deterministic.
    let (sink_id, _conn, _sink) = attested_sink(&server, &switchboard, &code).await;

    let mut sender = spawn_component(
        &a,
        "sender",
        "menu-backend",
        &[
            "files.send",
            "devices.read",
            "session.read",
            "transfers.read",
        ],
    )
    .await;
    subscribe_transfers(&mut sender).await;
    wait_server_connected(&mut sender, true).await;
    wait_reachable(&mut sender, &sink_id).await;

    // Bigger than the pipe buffer: the write blocks once it is full.
    let src = a.write_source("big.bin", &vec![7u8; 512 * 1024]);
    let r = sender
        .request(
            "files.send",
            json!({ "device_id": sink_id, "paths": [src.to_str().unwrap()] }),
        )
        .await
        .expect("files.send");
    let transfer_id = r["transfer_id"].as_str().expect("transfer_id").to_string();
    let started = sender.wait_notification("transfer.started").await;
    assert_eq!(started["transfer_id"], json!(transfer_id));

    sender
        .request("files.cancel", json!({ "transfer_id": transfer_id }))
        .await
        .expect("files.cancel");
    let failed = sender.wait_notification("transfer.failed").await;
    assert_eq!(failed["transfer_id"], json!(transfer_id));
    assert_eq!(failed["error"], json!("cancelled"));

    // Deregistered by the task BEFORE the terminal notification: re-cancelling
    // after seeing it → unknown.
    let err = sender
        .request("files.cancel", json!({ "transfer_id": transfer_id }))
        .await
        .expect_err("a finished transfer is no longer cancellable");
    assert_eq!(err.app_code(), "TRANSFER_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_an_unknown_transfer_is_reported() {
    let server = TestServer::start().await;
    let a = TestCore::start_enrolled_on(&server, &MemorySwitchboard::new()).await;
    let mut sender = spawn_component(&a, "sender", "menu-backend", &["files.send"]).await;
    let err = sender
        .request("files.cancel", json!({ "transfer_id": "t_nonexistent" }))
        .await
        .expect_err("unknown id");
    assert_eq!(err.app_code(), "TRANSFER_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn files_send_validates_paths_and_target() {
    let server = TestServer::start().await;
    let (a, b) = TestCore::start_pair(&server).await;
    let mut sender = spawn_component(
        &a,
        "sender",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    wait_server_connected(&mut sender, true).await;
    wait_reachable(&mut sender, b.device_id()).await;

    let good = a.write_source("ok.txt", b"x");
    let good = good.to_str().unwrap();

    // No file.
    let err = sender
        .request(
            "files.send",
            json!({ "device_id": b.device_id(), "paths": [] }),
        )
        .await
        .expect_err("empty list");
    assert_eq!(err.code, -32602, "{err:?}");

    // An unknown target (even before touching the disk).
    let err = sender
        .request(
            "files.send",
            json!({ "device_id": "d_unknown", "paths": [good] }),
        )
        .await
        .expect_err("unknown target");
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");

    // An absent path.
    let missing = a.config_dir().join("not-there.txt");
    let err = sender
        .request(
            "files.send",
            json!({ "device_id": b.device_id(), "paths": [missing.to_str().unwrap()] }),
        )
        .await
        .expect_err("absent path");
    assert_eq!(err.code, -32602, "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn files_send_requires_the_files_send_scope() {
    let server = TestServer::start().await;
    let a = TestCore::start_enrolled_on(&server, &MemorySwitchboard::new()).await;
    let mut c = spawn_component(&a, "no-scope", "tray", &["session.read"]).await;
    let err = c
        .request(
            "files.send",
            json!({ "device_id": "x", "paths": ["/tmp/x"] }),
        )
        .await
        .expect_err("files.send without the scope");
    assert_eq!(err.app_code(), "SCOPE_DENIED");
    let err = c
        .request("files.cancel", json!({ "transfer_id": "t_1" }))
        .await
        .expect_err("files.cancel without the scope");
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Subscribes a component to the `transfers` topic (it has the `transfers.read`
/// scope).
async fn subscribe_transfers(c: &mut TestComponent) {
    c.request("events.subscribe", json!({ "topics": ["transfers"] }))
        .await
        .expect("events.subscribe transfers");
}

/// Sends a file and returns the `transfer_id` (without waiting for the end).
async fn send_one(sender: &mut TestComponent, device_id: &str, src: &Path) -> String {
    let r = sender
        .request(
            "files.send",
            json!({ "device_id": device_id, "paths": [src.to_str().unwrap()] }),
        )
        .await
        .expect("files.send");
    r["transfer_id"].as_str().expect("transfer_id").to_string()
}

/// Sends `src` to `device_id`, waits for the end on the receiver side, and
/// checks the content written to its disk.
async fn transfer_and_expect(
    sender: &mut TestComponent,
    watcher: &mut TestComponent,
    device_id: &str,
    src: &Path,
    expected: &[u8],
) {
    send_one(sender, device_id, src).await;
    let finished = watcher.wait_notification("transfer.finished").await;
    let written = finished["paths"][0].as_str().expect("written path");
    assert_eq!(std::fs::read(written).expect("received file"), expected);
}

/// A raw endpoint attempts a transfer to a peer: that peer's Core must refuse
/// (peer_in_directory) — `send_transfer` fails, no ack.
async fn transfer_is_refused(rogue: &Arc<MemoryTransport>, node_id: &str, relay: &str) {
    let (_dir, src) = scratch_file(b"payload");
    let files = vec![OutgoingFile {
        name: "f.bin".into(),
        source: Some(src),
        size: b"payload".len() as u64,
        is_dir: false,
    }];
    let peer = PeerAddr {
        node_id: node_id.to_string(),
        relay_url: Some(relay.to_string()),
    };
    let mut stream = rogue
        .open(&peer)
        .await
        .expect("the switchboard routes: it is up to the Core to refuse");
    send_transfer(&mut stream, &files, &mut |_, _| {})
        .await
        .expect_err("a refused peer does not get an ack");
}

/// Writes a RAW offer (`(name, announced_size, body)` — outside of
/// `send_transfer`, which only produces safe basenames and exact sizes), the
/// bodies, then CLOSES the write half (EOF). Used to exercise the receiver on
/// names/sizes that the legitimate API would not produce.
async fn offer_then_close(stream: &mut Box<dyn IoStream>, files: &[(&str, u64, &[u8])]) {
    write_offer(stream, files).await;
    let _ = stream.shutdown().await;
    // The receiver rejects/fails and closes: we wait for the EOF, bounded.
    let mut sink = [0u8; 64];
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
}

/// Writes a raw offer + a PARTIAL body, then HOLDS the stream open (no
/// closure): the receiver stays blocked waiting for the rest — to exercise the
/// cancellation of an inbound transfer in progress.
async fn offer_then_hold(stream: &mut Box<dyn IoStream>, name: &str, size: u64, partial: &[u8]) {
    write_offer(stream, &[(name, size, partial)]).await;
}

async fn write_offer(stream: &mut Box<dyn IoStream>, files: &[(&str, u64, &[u8])]) {
    let manifest: Vec<Value> = files
        .iter()
        .map(|(n, size, _)| json!({ "name": n, "size": size }))
        .collect();
    let offer = serde_json::to_vec(&json!({ "type": "offer", "files": manifest })).unwrap();
    let _ = stream.write_all(&(offer.len() as u32).to_be_bytes()).await;
    let _ = stream.write_all(&offer).await;
    for (_, _, body) in files {
        let _ = stream.write_all(body).await;
    }
    let _ = stream.flush().await;
}

/// The files currently in a Core's receive directory (absent directory = empty
/// list).
fn received_files(core: &TestCore) -> Vec<PathBuf> {
    match std::fs::read_dir(core.receive_dir()) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
        Err(_) => Vec::new(),
    }
}

/// A temporary file to send (the `TempDir` must stay alive for the duration of
/// the send — hence returning the pair).
fn scratch_file(contents: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.bin");
    std::fs::write(&path, contents).expect("write the temporary file");
    (dir, path)
}

/// A temporary source TREE to send, returning `(guard, top)`:
///   top/a.txt = "alpha", top/sub/b.bin = "beta bytes", top/empty/ (empty).
/// The `TempDir` guard must outlive the send.
fn scratch_tree() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let top = dir.path().join("top");
    std::fs::create_dir_all(top.join("sub")).expect("create sub");
    std::fs::create_dir_all(top.join("empty")).expect("create empty dir");
    std::fs::write(top.join("a.txt"), b"alpha").expect("write a.txt");
    std::fs::write(top.join("sub").join("b.bin"), b"beta bytes").expect("write b.bin");
    (dir, top)
}

/// A peer of the account (attested under `code`) whose transport ACCEPTS
/// streams but never reads them: the sender blocks once the buffer is full — a
/// transfer thus stays in flight, to test cancellation. The server connection
/// and the transport must stay alive (relay published, route registered).
async fn attested_sink(
    server: &TestServer,
    switchboard: &MemorySwitchboard,
    code: &str,
) -> (String, TestConn, Arc<MemoryTransport>) {
    let key = DeviceKey::generate();
    let mut conn = server.connect_direct().await;
    let device_id = enroll_key(
        &mut conn,
        &server.oidc,
        &key,
        TEST_SUB,
        "sink",
        std::env::consts::OS,
    )
    .await;
    authenticate(&mut conn, &key, &device_id).await;
    let ak = universallink_core::account_key::account_key_from_code(code).expect("valid code");
    let att = universallink_core::account_key::attest(&ak, &key.node_id());
    let relay = format!("iroh+memory://{}", key.node_id());
    conn.request(
        "presence.update",
        json!({ "attestation": att, "relay_url": relay }),
    )
    .await
    .expect("presence.update from the sink");
    let sink = switchboard.endpoint(key.node_id(), Some(relay));
    (device_id, conn, sink)
}

/// A device of the account (attested under `code`) that does NOT publish a
/// relay: present and attested in the directory, but unreachable — to exercise
/// `DEVICE_OFFLINE`. Its connection must stay alive (otherwise the attestation,
/// which survives the offline period, would remain but the device would leave
/// the directory).
async fn attested_without_relay(server: &TestServer, code: &str) -> (String, TestConn) {
    let key = DeviceKey::generate();
    let mut conn = server.connect_direct().await;
    let device_id = enroll_key(
        &mut conn,
        &server.oidc,
        &key,
        TEST_SUB,
        "no-relay",
        std::env::consts::OS,
    )
    .await;
    authenticate(&mut conn, &key, &device_id).await;
    let ak = universallink_core::account_key::account_key_from_code(code).expect("valid code");
    let att = universallink_core::account_key::attest(&ak, &key.node_id());
    // Attestation ONLY — no relay_url.
    conn.request("presence.update", json!({ "attestation": att }))
        .await
        .expect("presence.update without a relay");
    (device_id, conn)
}

/// The `PeerAddr` (synthetic memory relay) to reach a `TestCore` — what a raw
/// endpoint presents to open a stream to it.
fn victim_peer(core: &TestCore) -> PeerAddr {
    PeerAddr {
        node_id: core.key().node_id(),
        relay_url: Some(format!("iroh+memory://{}", core.key().node_id())),
    }
}
