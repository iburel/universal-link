// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Serverless beyond the LAN: a device's signed reach hints (`addrs`,
//! `relay_hint`) travel in the directory gossip, and a sibling that left the
//! local network stays dialable through them: no server, no third party.
//!
//! The in-memory switchboard is the assertion mechanism: off the fake LAN, a
//! stream opens ONLY if the caller presents an address the peer actually
//! declared (or its registered relay), exactly as strict as iroh. A transfer
//! that completes with both devices off the LAN therefore proves the hints
//! were learned from the gossip and presented at the dial; nothing else could
//! have connected.

use serde_json::json;

use onedevice_core::Reach;
use onedevice_test_support::memory_transport::MemorySwitchboard;

use crate::support::*;

/// The record `device_id` under a `devices.list` result.
fn device_in<'a>(list: &'a serde_json::Value, device_id: &str) -> &'a serde_json::Value {
    list.as_array()
        .expect("a list")
        .iter()
        .find(|d| d["device_id"].as_str() == Some(device_id))
        .unwrap_or_else(|| panic!("{device_id} is not in {list}"))
}

/// A serverless pair on one switchboard, each seeded with the other's record
/// (the pairing brick's job, presupposed here like in the dirsync suite),
/// with the keys returned, since the reach declarations name them.
async fn reachable_pair(
    code: &str,
    switchboard: &MemorySwitchboard,
    b_relay: Option<String>,
) -> (TestCore, TestCore) {
    let a_key = DeviceKey::generate();
    let b_key = DeviceKey::generate();
    let describe =
        |key: &DeviceKey| peer_record(key, CORE_DEVICE_NAME, std::env::consts::OS, code, 1);
    let a = TestCore::start_in_account_on(code, switchboard, a_key, &[describe(&b_key)]).await;
    let b = TestCore::start_in_account_reaching(
        code,
        switchboard,
        b_key,
        &[describe(a.key())],
        b_relay,
    )
    .await;
    (a, b)
}

/// The sender's component and the receiver's watcher, transfers subscribed.
async fn transfer_ends(a: &TestCore, b: &TestCore) -> (TestComponent, TestComponent) {
    let mut sender = spawn_component(
        a,
        "sender",
        "menu-backend",
        &["files.send", "devices.read", "session.read"],
    )
    .await;
    let mut watcher = spawn_component(
        b,
        "watcher",
        "tray",
        &["transfers.read", "devices.read", "session.read"],
    )
    .await;
    watcher
        .request("events.subscribe", json!({ "topics": ["transfers"] }))
        .await
        .expect("events.subscribe");
    // Reverse barrier (the mutual-attestation race): B refuses A's first
    // stream fail-closed unless it has attested A too.
    wait_attested(&mut sender, &b.node_id()).await;
    wait_attested(&mut watcher, &a.node_id()).await;
    (sender, watcher)
}

/// The whole point of #87: B declares where it can be dialed, A learns it
/// through the gossip while they share a LAN; and once NEITHER is on it, a
/// file still goes through, on B's signed word alone.
#[tokio::test(flavor = "multi_thread")]
async fn signed_hints_dial_a_sibling_that_left_the_lan() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let (a, b) = reachable_pair(&code, &switchboard, None).await;
    let (mut sender, mut watcher) = transfer_ends(&a, &b).await;

    // B's transport observes its addresses (a VPN came up, say): its Core
    // re-signs its record, and the gossip carries it to A.
    let declared = Reach {
        addrs: vec!["10.8.0.2:41641".to_string()],
        relay_hint: None,
    };
    switchboard.declare_reach(&b.node_id(), declared);
    wait_directory(
        &mut sender,
        &b.node_id(),
        |d| d["addrs"] == json!(["10.8.0.2:41641"]),
        "B's signed addresses to reach A through the gossip",
    )
    .await;
    // And the record still proves itself, hints included: that is the point.
    let list = sender
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let learned = device_in(&list, &b.node_id()).clone();
    assert!(
        onedevice_core::directory::verify_record(&learned),
        "{learned}"
    );
    assert_eq!(
        learned["reachable"],
        json!(true),
        "worth dialing on its own word: {learned}"
    );

    // Both leave the LAN: mDNS resolves nothing anymore. The declared address
    // is now the ONLY thing that can open a stream to B.
    switchboard.leave_lan(&a.node_id());
    switchboard.leave_lan(&b.node_id());

    let src = a.write_source("beyond.txt", b"across networks, no server");
    sender
        .request(
            "files.send",
            json!({ "device_id": b.node_id(), "paths": [src.to_str().unwrap()] }),
        )
        .await
        .expect("files.send through the signed hint");
    let finished = watcher.wait_notification("transfer.finished").await;
    let written = finished["paths"][0].as_str().expect("written path");
    assert_eq!(
        std::fs::read(written).expect("received file"),
        b"across networks, no server"
    );
}

/// The fail-closed half: the same topology without the declaration stays
/// exactly as unreachable as before the hints existed. Off the LAN, no hint =
/// `DEVICE_OFFLINE`, synchronous, never a dial into the void.
#[tokio::test(flavor = "multi_thread")]
async fn without_hints_a_sibling_off_the_lan_stays_offline() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let (a, b) = reachable_pair(&code, &switchboard, None).await;
    let (mut sender, _watcher) = transfer_ends(&a, &b).await;

    switchboard.leave_lan(&a.node_id());
    switchboard.leave_lan(&b.node_id());

    let src = a.write_source("nowhere.txt", b"x");
    let err = sender
        .request(
            "files.send",
            json!({ "device_id": b.node_id(), "paths": [src.to_str().unwrap()] }),
        )
        .await
        .expect_err("no LAN, no relay, no hints");
    assert_eq!(err.app_code(), "DEVICE_OFFLINE");
}

/// #89's core: a serverless device points its config at a self-hosted relay,
/// SIGNS that hint into its record, and a sibling that learned it from the
/// gossip dials through the relay: explicitly, never a silent default.
#[tokio::test(flavor = "multi_thread")]
async fn a_relay_hint_learned_from_the_gossip_dials_through_the_relay() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    // B's endpoint homes on its configured relay: the switchboard will route
    // a caller that PRESENTS that relay, LAN or not.
    let relay = "https://relay.self-hosted.example".to_string();
    let (a, b) = reachable_pair(&code, &switchboard, Some(relay.clone())).await;
    let (mut sender, mut watcher) = transfer_ends(&a, &b).await;

    // What the daemon's transport does with a configured relay: claim it.
    switchboard.declare_reach(
        &b.node_id(),
        Reach {
            addrs: Vec::new(),
            relay_hint: Some(relay),
        },
    );
    wait_directory(
        &mut sender,
        &b.node_id(),
        |d| d["relay_hint"].as_str().is_some(),
        "B's signed relay hint to reach A through the gossip",
    )
    .await;

    switchboard.leave_lan(&a.node_id());
    switchboard.leave_lan(&b.node_id());

    let src = a.write_source("relayed.txt", b"rendezvous at the relay");
    sender
        .request(
            "files.send",
            json!({ "device_id": b.node_id(), "paths": [src.to_str().unwrap()] }),
        )
        .await
        .expect("files.send through the signed relay hint");
    let finished = watcher.wait_notification("transfer.finished").await;
    let written = finished["paths"][0].as_str().expect("written path");
    assert_eq!(
        std::fs::read(written).expect("received file"),
        b"rendezvous at the relay"
    );
}

/// A reach that MOVES reaches the account through the nudge; twice, because
/// the first declaration could ride a round the LAN join already triggered;
/// once A has seen the first addresses, only the nudge can carry the second.
#[tokio::test(flavor = "multi_thread")]
async fn a_reach_change_supersedes_the_hints_the_account_held() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let (a, b) = reachable_pair(&code, &switchboard, None).await;
    let mut sender = spawn_component(
        &a,
        "gui",
        "gui",
        &["session.read", "devices.read", "devices.manage"],
    )
    .await;

    let mut declare = async |addr: &str| {
        switchboard.declare_reach(
            &b.node_id(),
            Reach {
                addrs: vec![addr.to_string()],
                relay_hint: None,
            },
        );
        wait_directory(
            &mut sender,
            &b.node_id(),
            |d| d["addrs"] == json!([addr]),
            &format!("{addr} to reach the other device"),
        )
        .await;
    };
    declare("10.8.0.2:41641").await;
    declare("192.0.2.9:41641").await;

    // The second description superseded the first: one list, not a merge,
    // and a record that still proves itself.
    let list = sender
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let learned = device_in(&list, &b.node_id());
    assert_eq!(learned["addrs"], json!(["192.0.2.9:41641"]));
    assert!(
        onedevice_core::directory::verify_record(learned),
        "{learned}"
    );
}

/// Joining is the one moment a reach becomes signable with the transport
/// having observed nothing new: a machine whose addresses were known BEFORE
/// it had any account signs them the moment `account.setup` gives it a
/// description to stand behind, without waiting for a network change.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_joins_signs_the_reach_it_already_observed() {
    let switchboard = MemorySwitchboard::new();
    let key = DeviceKey::generate();
    let node_id = key.node_id();
    let core = TestCore::start_fresh_on(&switchboard, key).await;
    // Observed while the machine had no account at all.
    switchboard.declare_reach(
        &node_id,
        Reach {
            addrs: vec!["10.8.0.4:41641".to_string()],
            relay_hint: None,
        },
    );

    let mut c = spawn_component(
        &core,
        "gui",
        "gui",
        &["session.manage", "session.read", "devices.read"],
    )
    .await;
    c.request("account.setup", json!({}))
        .await
        .expect("account.setup with no server");

    wait_directory(
        &mut c,
        &node_id,
        |d| d["addrs"] == json!(["10.8.0.4:41641"]) && onedevice_core::directory::verify_record(d),
        "the pre-observed reach to be signed into our own record",
    )
    .await;
}

/// A machine with more addresses than a record may carry (a Docker host grows
/// one bridge per project) still signs: the claim is clamped at the signing
/// boundary, not refused. Unclamped, the whole reach machinery would silently
/// freeze on exactly the machines with the most networks.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_with_too_many_addresses_still_signs_a_clamped_reach() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let (a, b) = reachable_pair(&code, &switchboard, None).await;
    let mut sender = spawn_component(
        &a,
        "gui",
        "gui",
        &["session.read", "devices.read", "devices.manage"],
    )
    .await;

    let flood: Vec<String> = (0..24).map(|n| format!("10.8.{n}.2:41641")).collect();
    switchboard.declare_reach(
        &b.node_id(),
        Reach {
            addrs: flood.clone(),
            relay_hint: None,
        },
    );

    wait_directory(
        &mut sender,
        &b.node_id(),
        |d| {
            d["addrs"].as_array().is_some_and(|a| a.len() == 16)
                && onedevice_core::directory::verify_record(d)
        },
        "a clamped, still-proven reach to arrive",
    )
    .await;
    let list = sender
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let learned = device_in(&list, &b.node_id());
    // The first sixteen of the claim, in order: deterministic, not arbitrary.
    assert_eq!(learned["addrs"], json!(flood[..16].to_vec()));
}
