// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! A device's first introduction, with no server anywhere: one machine shows a
//! code, the other dials it on the local network (building block 4).
//!
//! This is the brick that makes the previous one visible. Until now a serverless
//! account was joined by typing the recovery code into every machine, which leaves
//! each of them knowing only itself — and the exchange that carries the directory
//! between devices (`dirsync.rs`) needs each to already hold the other's record. So
//! every test here ends the same way: the two devices know each other, and the
//! sync task takes over.
//!
//! What holds it up is written in `pairing.rs`. What is checked here is what a
//! caller sees: the same four `pairing.*` methods and the same four notifications
//! as through a server, and the refusals — a dialer that never saw the screen, two
//! devices of two different accounts, two devices with no account at all.

use onedevice_core::{FileSecretStore, PeerAddr, PeerTransport, SecretStore};
use onedevice_test_support::memory_transport::MemorySwitchboard;
use serde_json::{Value, json};

use crate::support::*;

/// A GUI-shaped component: pairs, manages the account, reads the directory.
async fn manager(core: &TestCore) -> TestComponent {
    spawn_component(
        core,
        "gui",
        "gui",
        &[
            "session.manage",
            "session.read",
            "devices.read",
            "devices.manage",
        ],
    )
    .await
}

/// A manager watching both topics a pairing shows up on.
async fn watching(core: &TestCore) -> TestComponent {
    let mut c = manager(core).await;
    c.request(
        "events.subscribe",
        json!({ "topics": ["pairing", "devices"] }),
    )
    .await
    .expect("events.subscribe");
    c
}

/// The `node_id`s a Core currently serves in its directory.
async fn known(c: &mut TestComponent) -> Vec<String> {
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    list.as_array()
        .expect("a list")
        .iter()
        .map(|d| d["node_id"].as_str().expect("node_id").to_string())
        .collect()
}

async fn wait_known(c: &mut TestComponent, node_id: &str) {
    let wanted = node_id.to_string();
    eventually(
        async || known(c).await.contains(&wanted),
        &format!("{node_id} to be known"),
    )
    .await;
}

async fn account_status(c: &mut TestComponent) -> Value {
    c.request("account.status", json!({}))
        .await
        .expect("account.status")
}

/// Shows a code on `from` and reads it on `to`, returning both halves. The two
/// gestures a human makes, and the only thing that differs between the tests below
/// is which device is which.
async fn hand_over(
    from: &mut TestComponent,
    to: &mut TestComponent,
) -> (Value, Result<Value, RpcError>) {
    let offer = from
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer with no server");
    let code = offer["code"].as_str().expect("a code").to_string();
    let claim = to.request("pairing.accept", json!({ "code": code })).await;
    (offer, claim)
}

/// The number the human is asked to compare, and the record they are shown: the
/// sponsor's `pairing.claimed`.
async fn claimed_on(c: &mut TestComponent, pairing_id: &str) -> Value {
    let claimed = c.wait_notification("pairing.claimed").await;
    assert_eq!(claimed["pairing_id"], json!(pairing_id));
    claimed
}

/// The whole point: a machine that has just been switched on joins the account
/// from a machine in the same room, and the two of them come out of it knowing
/// each other — which is what makes the directory exchange run at all.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_device_joins_the_account_over_the_local_network() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let fresh = TestCore::start_fresh_on(&switchboard, DeviceKey::generate()).await;
    let mut hc = watching(&holder).await;
    let mut fc = watching(&fresh).await;

    let offer = hc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer with no server");
    let pairing_id = offer["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string();
    let code_shown = offer["code"].as_str().expect("a code").to_string();
    assert!(
        code_shown.starts_with("1D2:"),
        "a code for a device to dial, not for a server to relay: {code_shown}"
    );
    assert!(
        code_shown.ends_with(&holder.node_id()),
        "and it says whom to dial: {code_shown}"
    );
    assert!(
        switchboard.listened(&holder.node_id()),
        "a code on screen is a device that has to be dialable: the daemon's \
         transport binds nothing until it is asked"
    );
    assert_eq!(offer["role"], json!("sponsor"), "it holds the account key");
    assert!(offer["expires_in"].as_u64().is_some_and(|s| s > 0));

    let claim = fc
        .request("pairing.accept", json!({ "code": code_shown }))
        .await
        .expect("pairing.accept of a LAN code");

    assert_eq!(claim["role"], json!("joiner"), "it has no account to give");
    assert!(
        claim.get("device").is_none(),
        "a joiner has nothing to confirm: {claim}"
    );
    // The sponsor is the one asked, and both ends show the same number.
    let claimed = claimed_on(&mut hc, &pairing_id).await;
    assert_eq!(
        claimed["verification"], claim["verification"],
        "the two ends of one channel, or the human is comparing nothing"
    );
    assert_eq!(claimed["device"]["node_id"], json!(fresh.node_id()));
    assert_eq!(claimed["device"]["name"], json!(CORE_DEVICE_NAME));
    assert_eq!(claimed["device"]["platform"], json!(std::env::consts::OS));
    // Nothing has crossed yet: the human has not answered.
    assert_eq!(account_status(&mut fc).await["attested"], json!(false));

    let confirmed = hc
        .request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect("pairing.confirm");
    assert_eq!(confirmed["status"], json!("done"));

    let done = fc.wait_notification("pairing.completed").await;
    assert_eq!(done["pairing_id"], claim["pairing_id"]);
    hc.wait_notification("pairing.completed").await;

    // In the account, holding its key — the same account, under the same
    // fingerprint, which is what a human would compare out of band.
    let joined = account_status(&mut fc).await;
    assert_eq!(joined["attested"], json!(true));
    assert_eq!(joined["holds_key"], json!(true));
    assert_eq!(
        joined["fingerprint"],
        account_status(&mut hc).await["fingerprint"]
    );

    // And they know each other, which no pairing through a server would have to
    // arrange: there the directory is the server's.
    wait_known(&mut fc, &holder.node_id()).await;
    wait_known(&mut hc, &fresh.node_id()).await;

    // The introduction is real, not just two records: the new device renames
    // itself and the other one hears about it — the sync brick, running for the
    // first time between two devices that had never met.
    fc.request(
        "devices.rename",
        json!({ "device_id": fresh.node_id(), "name": "Atelier" }),
    )
    .await
    .expect("devices.rename with no server");
    eventually(
        async || {
            let list = hc
                .request("devices.list", json!({}))
                .await
                .expect("devices.list");
            find_device(&list, &fresh.node_id())["name"] == json!("Atelier")
        },
        "the new device's name to reach the one that vouched for it",
    )
    .await;

    // And none of it was a runtime accident: the new device comes back from disk
    // in the account, still knowing the device that vouched for it.
    let fresh = fresh.restart().await;
    let mut fc = manager(&fresh).await;
    assert_eq!(account_status(&mut fc).await["attested"], json!(true));
    assert!(known(&mut fc).await.contains(&holder.node_id()));
}

/// A code read on the very machine that is displaying it is not a pairing. Left
/// unchecked it would be a device dialling itself, which the transport is only too
/// happy to allow.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_does_not_pair_with_itself() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let core = TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let mut c = watching(&core).await;

    let offer = c
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let refused = c
        .request("pairing.accept", json!({ "code": offer["code"] }))
        .await
        .expect_err("its own code");

    assert_eq!(refused.code, -32602, "{refused:?}");
    c.assert_silent().await;
    assert_eq!(known(&mut c).await, vec![core.node_id()]);
}

/// The other gesture, and the one a machine with no camera needs: the NEW device
/// shows the code and the one that has the account reads it. The dialling is
/// reversed; who confirms is not — the human is asked on the side that gives.
#[tokio::test(flavor = "multi_thread")]
async fn the_new_device_can_be_the_one_showing_the_code() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let fresh = TestCore::start_fresh_on(&switchboard, DeviceKey::generate()).await;
    let mut hc = watching(&holder).await;
    let mut fc = watching(&fresh).await;

    let (offer, claim) = hand_over(&mut fc, &mut hc).await;
    let claim = claim.expect("pairing.accept on the device that has the account");

    assert_eq!(offer["role"], json!("joiner"), "it has nothing to give yet");
    assert_eq!(
        claim["role"],
        json!("sponsor"),
        "it is the one that vouches"
    );
    assert_eq!(
        claim["device"]["node_id"],
        json!(fresh.node_id()),
        "and it is shown what it is being asked to admit: {claim}"
    );
    // The joining side is told the number too, so the human can compare.
    let claimed = claimed_on(&mut fc, offer["pairing_id"].as_str().expect("pairing_id")).await;
    assert_eq!(claimed["verification"], claim["verification"]);
    assert!(
        claimed.get("device").is_none(),
        "the joining side is not asked to decide: {claimed}"
    );

    hc.request(
        "pairing.confirm",
        json!({ "pairing_id": claim["pairing_id"] }),
    )
    .await
    .expect("pairing.confirm");

    fc.wait_notification("pairing.completed").await;
    hc.wait_notification("pairing.completed").await;
    assert_eq!(account_status(&mut fc).await["holds_key"], json!(true));
    wait_known(&mut fc, &holder.node_id()).await;
    wait_known(&mut hc, &fresh.node_id()).await;
}

/// Two devices that ALREADY share the account and have never met — which is what a
/// serverless account looks like when the recovery code was typed into each
/// machine in turn. There is no key to hand over, and the exchange is the same one:
/// what they are really doing is swapping directories.
#[tokio::test(flavor = "multi_thread")]
async fn two_devices_of_one_account_that_never_met_introduce_themselves() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    // A device each has heard of and the other has not: BOTH have to cross, or the
    // introduction is only to each other and not to the account. One direction rides
    // the grant, the other the answer to it.
    let hers = DeviceKey::generate();
    let his = DeviceKey::generate();
    let a = TestCore::start_in_account_on(
        &code,
        &switchboard,
        DeviceKey::generate(),
        &[peer_record(&hers, "Phone", "android", &code, 4)],
    )
    .await;
    let b = TestCore::start_in_account_on(
        &code,
        &switchboard,
        DeviceKey::generate(),
        &[peer_record(&his, "Tablet", "linux", &code, 4)],
    )
    .await;
    let mut ca = watching(&a).await;
    let mut cb = watching(&b).await;
    let before = account_status(&mut cb).await;

    let (offer, claim) = hand_over(&mut ca, &mut cb).await;
    let claim = claim.expect("pairing.accept between two devices of one account");

    // Both hold the key: the device that displayed the code is the one asked.
    assert_eq!(offer["role"], json!("sponsor"));
    assert_eq!(claim["role"], json!("joiner"));
    claimed_on(&mut ca, offer["pairing_id"].as_str().expect("pairing_id")).await;
    ca.request(
        "pairing.confirm",
        json!({ "pairing_id": offer["pairing_id"] }),
    )
    .await
    .expect("pairing.confirm");

    cb.wait_notification("pairing.completed").await;
    ca.wait_notification("pairing.completed").await;

    // The account did not move: same key, same fingerprint. Re-deriving the key it
    // already holds is what `account_key::install` treats as the no-op it is.
    assert_eq!(account_status(&mut cb).await, before);
    // They know each other, and each has learned what only the other held: the
    // sponsor's roster travelled with the grant, the joiner's came back after it.
    wait_known(&mut cb, &a.node_id()).await;
    wait_known(&mut ca, &b.node_id()).await;
    wait_known(&mut cb, &hers.node_id()).await;
    wait_known(&mut ca, &his.node_id()).await;
}

/// A device that has the account but not its key gets it back this way — the
/// second door `account_key` names, next to the recovery code typed in again.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_lost_its_account_key_gets_it_back() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let keyless =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    // The state `attested: true, holds_key: false` describes: in the account, and
    // unable to sign for it.
    FileSecretStore::new(keyless.config_dir()).delete("account-key-seed");
    // A store stamped well in the past, to see whether this path moves the
    // freshness bound. It must not: nothing here was checked against a server, and
    // that stamp is a bound against one.
    let store = keyless.config_dir().join("directory.json");
    let stamp = 1_000_000_u64;
    let mut on_disk: Value =
        serde_json::from_slice(&std::fs::read(&store).expect("the store")).expect("JSON");
    on_disk["saved_at"] = json!(stamp);
    std::fs::write(&store, on_disk.to_string()).expect("re-stamp the store");
    let mut hc = watching(&holder).await;
    let mut kc = watching(&keyless).await;
    let status = account_status(&mut kc).await;
    assert_eq!(status["attested"], json!(true));
    assert_eq!(status["holds_key"], json!(false));

    // It shows the code, as a device asking for something does.
    let (offer, claim) = hand_over(&mut kc, &mut hc).await;
    let claim = claim.expect("pairing.accept");
    assert_eq!(offer["role"], json!("joiner"), "it cannot vouch for anyone");
    assert_eq!(claim["role"], json!("sponsor"));

    hc.request(
        "pairing.confirm",
        json!({ "pairing_id": claim["pairing_id"] }),
    )
    .await
    .expect("pairing.confirm");
    kc.wait_notification("pairing.completed").await;

    let after = account_status(&mut kc).await;
    assert_eq!(after["holds_key"], json!(true), "it can vouch now");
    assert_eq!(
        after["fingerprint"], status["fingerprint"],
        "and it is the same account it was already in"
    );
    // What it learned reached the disk — and the freshness stamp did not move.
    wait_known(&mut kc, &holder.node_id()).await;
    let written: Value =
        serde_json::from_slice(&std::fs::read(&store).expect("the store")).expect("JSON");
    assert!(
        written["devices"].get(holder.node_id()).is_some(),
        "the device that vouched for it is on disk: {written}"
    );
    assert_eq!(written["saved_at"], json!(stamp), "{written}");
}

/// Neither device able to vouch: two machines with no account do not make one by
/// meeting. Refused before anything is displayed to a human.
#[tokio::test(flavor = "multi_thread")]
async fn two_devices_with_no_account_get_nowhere() {
    let switchboard = MemorySwitchboard::new();
    let a = TestCore::start_fresh_on(&switchboard, DeviceKey::generate()).await;
    let b = TestCore::start_fresh_on(&switchboard, DeviceKey::generate()).await;
    let mut ca = watching(&a).await;
    let mut cb = watching(&b).await;

    let (offer, claim) = hand_over(&mut ca, &mut cb).await;

    assert_eq!(
        claim.expect_err("nobody here can vouch").app_code(),
        "NO_ACCOUNT_KEY"
    );
    // And the device showing the code is told, rather than left waiting out its
    // deadline with a code on screen.
    let failed = ca.wait_notification("pairing.failed").await;
    assert_eq!(failed["pairing_id"], offer["pairing_id"]);
    assert_eq!(failed["reason"], json!("no_account"));
    assert_eq!(account_status(&mut cb).await["attested"], json!(false));
}

/// Two accounts do not become one by pairing. Refused BEFORE the seed crosses —
/// the install would refuse it anyway, and by then a key would have been handed to
/// a device of somebody else's account.
#[tokio::test(flavor = "multi_thread")]
async fn two_accounts_are_not_merged_by_a_pairing() {
    let switchboard = MemorySwitchboard::new();
    let ours = onedevice_core::account_key::generate_recovery_code();
    let theirs = onedevice_core::account_key::generate_recovery_code();
    let a = TestCore::start_in_account_on(&ours, &switchboard, DeviceKey::generate(), &[]).await;
    let b = TestCore::start_in_account_on(&theirs, &switchboard, DeviceKey::generate(), &[]).await;
    let mut ca = watching(&a).await;
    let mut cb = watching(&b).await;
    let before = account_status(&mut cb).await;

    let (offer, claim) = hand_over(&mut ca, &mut cb).await;

    assert_eq!(
        claim.expect_err("another account entirely").app_code(),
        "ACCOUNT_KEY_SET"
    );
    let failed = ca.wait_notification("pairing.failed").await;
    assert_eq!(failed["pairing_id"], offer["pairing_id"]);
    assert_eq!(failed["reason"], json!("other_account"));
    // Neither side learned anything about the other.
    assert_eq!(account_status(&mut cb).await, before);
    assert!(!known(&mut ca).await.contains(&b.node_id()));
    assert!(!known(&mut cb).await.contains(&a.node_id()));
}

/// The human says no: the account stays where it is, the other device is told, and
/// the device that asked did not slip into the directory on the way past.
#[tokio::test(flavor = "multi_thread")]
async fn a_pairing_the_human_declines_hands_nothing_over() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let fresh = TestCore::start_fresh_on(&switchboard, DeviceKey::generate()).await;
    let mut hc = watching(&holder).await;
    let mut fc = watching(&fresh).await;

    let offer = hc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let claim = fc
        .request("pairing.accept", json!({ "code": offer["code"] }))
        .await
        .expect("pairing.accept");
    claimed_on(&mut hc, offer["pairing_id"].as_str().expect("pairing_id")).await;

    hc.request(
        "pairing.cancel",
        json!({ "pairing_id": offer["pairing_id"] }),
    )
    .await
    .expect("pairing.cancel");

    let failed = fc.wait_notification("pairing.failed").await;
    assert_eq!(failed["pairing_id"], claim["pairing_id"]);
    assert_eq!(failed["reason"], json!("declined"));
    assert_eq!(account_status(&mut fc).await["attested"], json!(false));
    assert!(
        !known(&mut hc).await.contains(&fresh.node_id()),
        "a device nobody vouched for is not in the directory"
    );
}

/// The same code read twice — a user who taps twice, or retries — must not take
/// down the pairing already under way. A window serves ONE dialer, so the second
/// dial finds a device that is no longer reading from strangers at all, and the
/// human on the other device still has the dialog the first one opened.
#[tokio::test(flavor = "multi_thread")]
async fn reading_the_same_code_twice_does_not_undo_the_first() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let fresh = TestCore::start_fresh_on(&switchboard, DeviceKey::generate()).await;
    let mut hc = watching(&holder).await;
    let mut fc = watching(&fresh).await;

    let offer = hc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let pairing_id = offer["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string();
    fc.request("pairing.accept", json!({ "code": offer["code"] }))
        .await
        .expect("pairing.accept");
    claimed_on(&mut hc, &pairing_id).await;

    let again = fc
        .request("pairing.accept", json!({ "code": offer["code"] }))
        .await
        .expect_err("that window already has its dialer");

    // "It did not answer", which is what happened: the window shut behind the first
    // dial, and this device is a stranger to it until the pairing goes through.
    assert_eq!(again.app_code(), "DEVICE_OFFLINE");
    // The pairing the human is looking at heard nothing about it.
    hc.assert_silent().await;
    hc.request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect("the first pairing is still confirmable");
    hc.wait_notification("pairing.completed").await;
    wait_known(&mut hc, &fresh.node_id()).await;
}

/// A code is spent by the window it belongs to: showing a second one retires the
/// first, and the first no longer opens anything — the proof is bound to the very
/// exchange on screen. The second one still works, so retiring is not breaking.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_code_retires_the_first() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let fresh = TestCore::start_fresh_on(&switchboard, DeviceKey::generate()).await;
    let mut hc = watching(&holder).await;
    let mut fc = manager(&fresh).await;

    let first = hc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let second = hc
        .request("pairing.offer", json!({}))
        .await
        .expect("a second pairing.offer");
    assert_ne!(first["code"], second["code"]);

    let stale = fc
        .request("pairing.accept", json!({ "code": first["code"] }))
        .await
        .expect_err("the code on screen is the second one");
    assert_eq!(stale.app_code(), "PAIRING_STATE");

    fc.request("pairing.accept", json!({ "code": second["code"] }))
        .await
        .expect("the code that IS on screen");
}

/// A `1D1` code names a rendezvous on a server, and a device with no server in
/// its life has no way to go to it. (The mirror refusal this test used to pin —
/// `1D2` on a device that answers to a server, `PAIRING_VIA_SERVER` — fell with
/// the continuum: such a device now sponsors over the local network, which
/// `continuum.rs` proves end to end.)
#[tokio::test(flavor = "multi_thread")]
async fn a_server_code_is_refused_where_no_server_answers() {
    let server = TestServer::start().await;
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let serverless =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let deployed = TestCore::start_with_server(&server).await;
    let mut sc = manager(&serverless).await;
    let mut dc = manager(&deployed).await;

    let server_code = dc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer through the server")["code"]
        .clone();
    assert!(
        server_code.as_str().is_some_and(|c| c.starts_with("1D1:")),
        "{server_code}"
    );
    let refused = sc
        .request("pairing.accept", json!({ "code": server_code }))
        .await
        .expect_err("no rendezvous to go to");
    assert_eq!(refused.app_code(), "SERVER_UNREACHABLE");
}

// ---------------------------------------------------------------------------
// A device that speaks the protocol by hand — the refusals a real Core would
// never produce.
// ---------------------------------------------------------------------------

/// A device on the network that is in nobody's directory, dialling the data plane
/// itself. `pub(crate)`: the leave tests (`leave.rs`) reuse it — as the struck-off
/// device whose dial is answered its tombstone, and (its record seeded) as the
/// compromised member whose forged rosters must wipe nothing.
pub(crate) struct Stranger {
    pub(crate) key: DeviceKey,
    transport: std::sync::Arc<onedevice_test_support::memory_transport::MemoryTransport>,
}

impl Stranger {
    pub(crate) fn join(switchboard: &MemorySwitchboard) -> Stranger {
        let key = DeviceKey::generate();
        let transport = switchboard.endpoint(key.node_id(), None);
        switchboard.join_lan(&key.node_id());
        Stranger { key, transport }
    }

    /// Sends `frame` to `target` and returns its answer — `None` when the Core
    /// closed the stream without a word, which is how it refuses a device it will
    /// not talk to.
    pub(crate) async fn say(&self, target: &TestCore, frame: Value) -> Option<Value> {
        let peer = PeerAddr {
            node_id: target.node_id(),
            relay_url: None,
            addrs: Vec::new(),
        };
        let mut stream = self
            .transport
            .open(&peer)
            .await
            .expect("the switchboard routes");
        peer_write(&mut stream, &frame).await;
        peer_read(&mut stream).await
    }

    /// Its own signed description, as a legitimate device would declare it. The
    /// attestation in it is beside the point: a declaration is checked against the
    /// key that sent it, and the account key is what the SPONSOR adds.
    pub(crate) fn record(&self, code: &str) -> Value {
        peer_record(&self.key, "Stranger", std::env::consts::OS, code, 1)
    }

    /// A code that names THIS device, so a real Core will dial it: the shape a
    /// serverless `pairing.offer` mints. What the two halves derive is nobody's
    /// business here — every test that dials a stranger is about the answer.
    pub(crate) fn code(&self) -> String {
        // 16 bytes of "optical secret" and 32 of public key, which is what the
        // fields are: a code of any other shape is not a code at all, and would
        // never get as far as being answered.
        format!(
            "1D2:{}:{}:{}",
            "c2NyZWVuLXNlY3JldC0xNg",
            STRANGER_EPK,
            self.key.node_id()
        )
    }

    /// Waits to be dialled, answers `reply` verbatim, and returns the offer it was
    /// sent — the dialling half of the protocol seen from the other side.
    async fn answer(&self, reply: Value) -> Value {
        let (offer, mut stream) = self.answer_keeping(reply).await;
        // Held until the dialer is done with it.
        let _ = peer_read(&mut stream).await;
        offer
    }

    /// `answer`, handing the stream back: for the tests that go on speaking (or
    /// deliberately stop) after the roles are settled.
    async fn answer_keeping(&self, reply: Value) -> (Value, Box<dyn onedevice_core::IoStream>) {
        let (_peer, mut stream) = tokio::time::timeout(RESPONSE_TIMEOUT, self.transport.accept())
            .await
            .expect("a Core to dial us")
            .expect("a stream");
        let offer = peer_read(&mut stream).await.expect("its offer");
        peer_write(&mut stream, &reply).await;
        (offer, stream)
    }
}

/// A well-formed public half for a dialer's channel: any 32 bytes are a usable
/// X25519 public key, and what these ones derive is nobody's business — every test
/// that uses them is about a frame being refused before the exchange matters.
const STRANGER_EPK: &str = "b3ZlciBteSBzaG91bGRlciwgbm90IHRoZSBzY3JlZW4";

/// The gate that keeps the data plane shut: with no pairing window open, a device
/// outside the directory gets nothing — not an answer, not a frame read. And the
/// window is exactly that narrow: while one IS open, anything other than a pairing
/// offer still gets nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_outside_the_directory_is_served_nothing_else() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let stranger = Stranger::join(&switchboard);
    let mut hc = manager(&holder).await;

    // No window: even the pairing frame is refused before it is read.
    let offer = json!({
        "type": "lan_pair", "epk": STRANGER_EPK, "proof": "de".repeat(32),
        "record": stranger.record(&code), "holds_key": false,
    });
    assert!(
        stranger.say(&holder, offer.clone()).await.is_none(),
        "a stranger got an answer with no pairing window open"
    );

    // A window open changes what is served, and only that: a transfer offer from a
    // device the account never attested is refused exactly as before.
    hc.request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let transfer = json!({ "type": "offer", "files": [{ "name": "x", "size": 0 }] });
    assert!(
        stranger.say(&holder, transfer).await.is_none(),
        "a stranger was served something other than a pairing offer"
    );
    // Nor is a frame that is not a pairing offer in any readable sense: without
    // channel material there is no conversation, and an answer would only tell
    // whoever is probing that something here is listening.
    for unreadable in [
        json!({ "type": "lan_pair" }),
        json!({ "type": "lan_pair", "epk": "AA", "proof": "00" }),
        json!({ "type": "lan_pair", "epk": STRANGER_EPK }),
    ] {
        assert!(
            stranger.say(&holder, unreadable.clone()).await.is_none(),
            "answered an unreadable offer: {unreadable}"
        );
    }
    // And the window is still there for the device the human is holding.
    assert!(
        stranger.say(&holder, offer).await.is_some(),
        "the pairing frame is the one that IS served"
    );
}

/// A dialer that never saw the screen is turned away, and — the half that matters —
/// the window SURVIVES it. Checking the proof before the displaying side spends its
/// ephemeral secret is what stops anything on the network from burning a pairing
/// window for the device the human is actually holding.
#[tokio::test(flavor = "multi_thread")]
async fn a_dialer_that_never_saw_the_code_cannot_burn_the_window() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let fresh = TestCore::start_fresh_on(&switchboard, DeviceKey::generate()).await;
    let stranger = Stranger::join(&switchboard);
    let mut hc = watching(&holder).await;
    let mut fc = manager(&fresh).await;

    let offer = hc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");

    // A well-formed offer in every respect but the one that counts.
    let answer = stranger
        .say(
            &holder,
            json!({
                "type": "lan_pair",
                "epk": STRANGER_EPK,
                "proof": "de".repeat(32),
                "record": stranger.record(&code),
                "holds_key": false,
            }),
        )
        .await
        .expect("a refusal, not silence");
    assert_eq!(answer["type"], json!("lan_refused"));
    assert_eq!(answer["reason"], json!("proof"));

    // The pairing is untouched: no failure was announced, and the code still works
    // for the device that did read it.
    hc.assert_silent().await;
    fc.request("pairing.accept", json!({ "code": offer["code"] }))
        .await
        .expect("the code the human carried is still good");
    claimed_on(&mut hc, offer["pairing_id"].as_str().expect("pairing_id")).await;

    // And now the window is shut: it serves exactly one dialer, so a stranger is
    // back to getting nothing read from it at all.
    assert!(
        stranger
            .say(
                &holder,
                json!({
                    "type": "lan_pair", "epk": STRANGER_EPK, "proof": "de".repeat(32),
                    "record": stranger.record(&code), "holds_key": false,
                }),
            )
            .await
            .is_none(),
        "a window that has taken its dialer is still open to strangers"
    );
}

/// A device does not get to describe another one. The declaration is checked
/// against the `node_id` the transport authenticated, so a dialer cannot arrive
/// wearing a sibling's name — nor a description nobody signed, which could never
/// enter a directory anyway.
#[tokio::test(flavor = "multi_thread")]
async fn a_declaration_that_is_not_the_dialers_own_is_refused() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let stranger = Stranger::join(&switchboard);
    let mut hc = watching(&holder).await;
    let offer = hc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");

    // Somebody else's record, signed by somebody else.
    let elsewhere = DeviceKey::generate();
    let borrowed = peer_record(&elsewhere, "Not-Mine", "linux", &code, 1);
    // Its own `node_id`, and a description it never signed.
    let mut unsigned = stranger.record(&code);
    unsigned["name"] = json!("Renamed-In-Flight");

    for wrong in [borrowed, unsigned, json!({}), Value::Null] {
        let answer = stranger
            .say(
                &holder,
                json!({
                    "type": "lan_pair", "epk": STRANGER_EPK, "proof": "de".repeat(32),
                    "record": wrong, "holds_key": true,
                }),
            )
            .await
            .expect("a refusal");
        assert_eq!(answer["reason"], json!("record"), "accepted: {wrong}");
    }

    // Nothing entered the directory, and the window is still the human's.
    assert_eq!(known(&mut hc).await, vec![holder.node_id()]);
    hc.assert_silent().await;
    assert_eq!(
        hc.request(
            "pairing.cancel",
            json!({ "pairing_id": offer["pairing_id"] })
        )
        .await
        .expect("pairing.cancel"),
        json!({})
    );
}

/// What a device that read a code sends, seen from the device it dialled. The
/// dialling half is the one no live pair can check on its own: two real Cores would
/// converge through the answers alone, and half the frame could be missing without
/// a test noticing.
#[tokio::test(flavor = "multi_thread")]
async fn what_the_dialer_sends_is_everything_the_other_side_needs() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let stranger = Stranger::join(&switchboard);
    let mut hc = manager(&holder).await;

    // The Core reads a code naming the stranger, and dials it.
    let dialling = hc.request("pairing.accept", json!({ "code": stranger.code() }));
    let (claim, offer) = tokio::join!(
        dialling,
        stranger.answer(json!({ "type": "lan_refused", "reason": "no_account" })),
    );

    assert_eq!(offer["type"], json!("lan_pair"));
    assert!(
        offer["epk"].as_str().is_some_and(|epk| !epk.is_empty()),
        "its half of the channel: {offer}"
    );
    assert!(
        offer["proof"].as_str().is_some_and(|p| p.len() == 64),
        "and what proves it read the code: {offer}"
    );
    assert_eq!(
        offer["holds_key"],
        json!(true),
        "whether it can vouch, which is what settles the roles: {offer}"
    );
    assert!(
        offer["account"].as_str().is_some(),
        "and which account it is in, as a mark: {offer}"
    );
    let declared = offer["record"].clone();
    assert_eq!(declared["node_id"], json!(holder.node_id()));
    assert_eq!(declared["name"], json!(CORE_DEVICE_NAME));
    assert!(
        onedevice_core::directory::verify_record(&declared),
        "a description it stands behind, or the other side could not take it in: {declared}"
    );
    // And the refusal it was answered with reaches its caller in this API's words.
    assert_eq!(
        claim.expect_err("refused").app_code(),
        "NO_ACCOUNT_KEY",
        "the reason must survive the wire"
    );
}

/// The dialling side checks what it is answered, too: a device that declares a
/// description belonging to another key gets nothing from us — which is what stops
/// a device on the network from answering a dial by wearing a sibling's name.
#[tokio::test(flavor = "multi_thread")]
async fn a_dialled_device_that_declares_someone_elses_description_is_refused() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let stranger = Stranger::join(&switchboard);
    let elsewhere = DeviceKey::generate();
    let mut hc = watching(&holder).await;

    for answer in [
        // A record signed by a key that is not the one we dialled.
        json!({
            "type": "lan_hello", "holds_key": false,
            "record": peer_record(&elsewhere, "Not-Its-Own", "linux", &code, 1),
        }),
        // Its own node_id, and a description nobody signed.
        json!({
            "type": "lan_hello", "holds_key": false,
            "record": { "node_id": stranger.key.node_id(), "name": "Hearsay", "platform": "linux" },
        }),
        // Not an answer to this conversation at all.
        json!({ "type": "dir_roster", "records": [], "revoked": {} }),
    ] {
        let dialling = hc.request("pairing.accept", json!({ "code": stranger.code() }));
        let (claim, _offer) = tokio::join!(dialling, stranger.answer(answer.clone()));
        assert_eq!(
            claim.expect_err("refused").app_code(),
            "PAIRING_STATE",
            "accepted: {answer}"
        );
    }

    // Nothing entered the directory, and no pairing was ever announced.
    assert_eq!(known(&mut hc).await, vec![holder.node_id()]);
    hc.assert_silent().await;
}

/// A device that takes the account and then says nothing more is still one of ours.
/// The sponsor attests the description it was shown and takes it in the moment the
/// key is out, rather than waiting for a courtesy the other side may not manage —
/// otherwise a device whose stream died a breath too early would hold the account
/// and be a stranger to the only device that could introduce it.
///
/// It also shows what the grant is: the key AND everything the sponsor knows about
/// the account, in one frame, so a device that learned only the key would not find
/// itself in an account with nobody in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_takes_the_account_and_falls_silent_is_still_known() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let stranger = Stranger::join(&switchboard);
    let mut hc = watching(&holder).await;

    // The stranger displays a code; the device with the account reads it and dials.
    let dialling = hc.request("pairing.accept", json!({ "code": stranger.code() }));
    let (claim, (_offer, mut stream)) = tokio::join!(
        dialling,
        stranger.answer_keeping(json!({
            "type": "lan_hello", "holds_key": false, "record": stranger.record(&code),
        })),
    );
    let claim = claim.expect("pairing.accept");
    assert_eq!(claim["role"], json!("sponsor"));

    hc.request(
        "pairing.confirm",
        json!({ "pairing_id": claim["pairing_id"] }),
    )
    .await
    .expect("pairing.confirm");

    let grant = peer_read(&mut stream).await.expect("the grant");
    assert_eq!(grant["type"], json!("pair_grant"));
    assert!(
        grant["bundle"].as_str().is_some_and(|b| !b.is_empty()),
        "the account, sealed: {grant}"
    );
    let carried: Vec<&str> = grant["records"]
        .as_array()
        .expect("records")
        .iter()
        .map(|record| record["node_id"].as_str().expect("node_id"))
        .collect();
    assert!(
        carried.contains(&holder.node_id().as_str()),
        "and whom the account knows, starting with the device that vouched: {grant}"
    );

    // And now we say nothing at all — no roster, not a byte.
    hc.wait_notification("pairing.completed").await;
    wait_known(&mut hc, &stranger.key.node_id()).await;
    let list = hc
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let learned = find_device(&list, &stranger.key.node_id());
    assert_eq!(learned["name"], json!("Stranger"));
    assert!(
        onedevice_core::directory::verify_record(learned),
        "the description it signed, kept verbatim: {learned}"
    );
    assert!(
        onedevice_core::account_key::verify(
            &ak_pub(&code),
            learned["node_id"].as_str().expect("node_id"),
            learned["attestation"].as_str().expect("attestation"),
        ),
        "and attested under the account's key, not merely present: {learned}"
    );
}

/// The account's public key, as every device of it derives it from the code.
fn ak_pub(code: &str) -> String {
    onedevice_core::account_key::public_hex(
        &onedevice_core::account_key::account_key_from_code(code).expect("a valid test code"),
    )
}

/// The dialling side does not take the other's word for whether they are in the
/// same account: it compares the mark itself. Between two honest devices the
/// dialled one would have refused first — this is what keeps the answer from
/// depending on that.
#[tokio::test(flavor = "multi_thread")]
async fn a_dialled_device_claiming_another_account_is_refused_by_the_dialer() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let stranger = Stranger::join(&switchboard);
    let mut hc = watching(&holder).await;

    // A device that answers "I hold an account key too" and names an account that
    // is not this one. Any mark but the right one is another account: only the two
    // ends of one exchange can compute it, which is the point of sending a mark
    // rather than the key.
    let dialling = hc.request("pairing.accept", json!({ "code": stranger.code() }));
    let (claim, _offer) = tokio::join!(
        dialling,
        stranger.answer(json!({
            "type": "lan_hello", "holds_key": true, "account": "de".repeat(32),
            "record": stranger.record(&code),
        })),
    );

    assert_eq!(
        claim.expect_err("another account entirely").app_code(),
        "ACCOUNT_KEY_SET"
    );
    assert!(!known(&mut hc).await.contains(&stranger.key.node_id()));
    hc.assert_silent().await;
}

/// A pairing is not confirmable before there is anything to confirm, and not by
/// the side that has nothing to give. The same two refusals as through a server,
/// for the same two reasons.
#[tokio::test(flavor = "multi_thread")]
async fn there_is_nothing_to_confirm_until_a_device_has_dialled() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let holder =
        TestCore::start_in_account_on(&code, &switchboard, DeviceKey::generate(), &[]).await;
    let fresh = TestCore::start_fresh_on(&switchboard, DeviceKey::generate()).await;
    let mut hc = manager(&holder).await;
    let mut fc = manager(&fresh).await;

    let offer = hc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let too_early = hc
        .request(
            "pairing.confirm",
            json!({ "pairing_id": offer["pairing_id"] }),
        )
        .await
        .expect_err("nobody has dialled");
    assert_eq!(too_early.app_code(), "PAIRING_STATE");

    // A joiner has nothing to confirm either.
    let claim = fc
        .request("pairing.accept", json!({ "code": offer["code"] }))
        .await
        .expect("pairing.accept");
    let not_ours = fc
        .request(
            "pairing.confirm",
            json!({ "pairing_id": claim["pairing_id"] }),
        )
        .await
        .expect_err("a joiner does not confirm");
    assert_eq!(not_ours.app_code(), "PAIRING_STATE");
    // And a confirmation naming a pairing this device never had.
    let unknown = hc
        .request("pairing.confirm", json!({ "pairing_id": "p_nobody" }))
        .await
        .expect_err("unknown pairing");
    assert_eq!(unknown.app_code(), "PAIRING_UNKNOWN");
}
