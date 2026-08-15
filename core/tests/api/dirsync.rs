// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Two devices of the account telling each other whom they know — the exchange
//! that turns a set of devices sharing a key into an account (building block 3).
//!
//! Every scenario here is SERVERLESS and on the LAN only: no relay published, no
//! session, nothing configured. What carries the roster is the data plane, over
//! the frame type `dir_sync` on the existing ALPN — and what makes it worth
//! anything is that neither end trusts the other with the contents: a record must
//! be signed by the device it describes and attested under the account key, a
//! tombstone must be signed by that account key.
//!
//! The premise every test starts from is that both devices ALREADY hold each
//! other's record, because a stream between two devices requires each to have
//! attested the other (`peer_in_directory` / `account_peers`). Giving that first
//! introduction is the pairing brick's job, not this one's.

use std::sync::Arc;
use std::time::Duration;

use onedevice_core::{PeerAddr, PeerTransport};
use onedevice_test_support::memory_transport::{MemorySwitchboard, MemoryTransport};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::support::*;

/// A GUI-shaped component: reads the directory and manages the devices.
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

async fn subscribed(core: &TestCore) -> TestComponent {
    let mut c = manager(core).await;
    c.request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .expect("events.subscribe");
    c
}

/// The `device_id`s a Core currently serves. A plain list rather than
/// `find_device`, which panics on absence — half of these tests are about a
/// device NOT being there.
async fn known(c: &mut TestComponent) -> Vec<String> {
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    list.as_array()
        .expect("a list")
        .iter()
        .map(|d| d["device_id"].as_str().expect("device_id").to_string())
        .collect()
}

async fn record_of(c: &mut TestComponent, device_id: &str) -> Value {
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    list.as_array()
        .expect("a list")
        .iter()
        .find(|d| d["device_id"].as_str() == Some(device_id))
        .unwrap_or_else(|| panic!("{device_id} is not in {list}"))
        .clone()
}

/// Waits until a Core knows `device_id` — the sync is a background round, so
/// every assertion about what it taught converges rather than holds at once.
async fn wait_known(c: &mut TestComponent, device_id: &str) {
    let id = device_id.to_string();
    eventually(
        async || known(c).await.contains(&id),
        &format!("{device_id} to be known"),
    )
    .await;
}

async fn wait_unknown(c: &mut TestComponent, device_id: &str) {
    let id = device_id.to_string();
    eventually(
        async || !known(c).await.contains(&id),
        &format!("{device_id} to be gone"),
    )
    .await;
}

/// Two serverless Cores of the same account, on the same LAN, each already
/// holding the other's record — plus whatever `a_knows` / `b_knows` add to each
/// store. The keys are generated here so the records can be built before either
/// Core exists.
async fn lan_pair(code: &str, a_knows: &[Value], b_knows: &[Value]) -> (TestCore, TestCore) {
    let switchboard = MemorySwitchboard::new();
    let a_key = DeviceKey::generate();
    let b_key = DeviceKey::generate();
    let describe =
        |key: &DeviceKey| peer_record(key, CORE_DEVICE_NAME, std::env::consts::OS, code, 1);
    let mut for_a = vec![describe(&b_key)];
    for_a.extend_from_slice(a_knows);
    let mut for_b = vec![describe(&a_key)];
    for_b.extend_from_slice(b_knows);
    let a = TestCore::start_in_account_on(code, &switchboard, a_key, &for_a).await;
    let b = TestCore::start_in_account_on(code, &switchboard, b_key, &for_b).await;
    (a, b)
}

/// The whole point of the brick: a device learns of a sibling it has never met,
/// from one that has. Without this, pairing every device with every other would
/// be the user's job.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_learns_of_a_sibling_it_has_never_met() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let third = DeviceKey::generate();
    let c_record = peer_record(&third, "Phone", "android", &code, 4);
    let (a, b) = lan_pair(&code, &[c_record], &[]).await;
    let mut ca = subscribed(&a).await;
    let mut cb = subscribed(&b).await;

    // A device nobody here had heard of ARRIVES: `device.added`, not an update to
    // something that was never there.
    let added = cb.wait_notification("device.added").await;
    assert_eq!(added["device"]["device_id"], json!(third.node_id()));
    wait_known(&mut cb, &third.node_id()).await;

    // What arrives is the description that device signed — not a name the
    // relayer chose.
    let learned = record_of(&mut cb, &third.node_id()).await;
    assert_eq!(learned["name"], json!("Phone"));
    assert_eq!(learned["platform"], json!("android"));
    assert_eq!(learned["seq"], json!(4));
    assert!(
        onedevice_core::directory::verify_record(&learned),
        "and it still stands behind it: {learned}"
    );
    // Known is not reachable: this device is nowhere near us, and the record
    // carries no route we could vouch for.
    assert_eq!(learned["online"], json!(false));
    assert_eq!(learned["relay_url"], json!(null));
    assert_eq!(learned["lan"], json!(false));
    assert_eq!(learned["reachable"], json!(false));
    assert_eq!(learned["is_self"], json!(false));

    // A learns nothing it did not already know: three devices, not four.
    assert_eq!(known(&mut ca).await.len(), 3);
    assert_eq!(known(&mut cb).await.len(), 3);
    // And B knows it across a restart — the exchange reached the disk.
    let b = b.restart().await;
    let mut cb = manager(&b).await;
    assert!(known(&mut cb).await.contains(&third.node_id()));
}

/// A device renames itself with no server to ask, and the account hears about it
/// without waiting for the next tick: `devices.rename` nudges the round.
///
/// TWICE, and that is the whole point of the shape. A single rename could be
/// carried by a round the LAN had already triggered, which would pass whether or
/// not the nudge exists. Once the other end has seen the FIRST name, the round
/// that carried it is over and both tasks are parked — so only a nudge can carry
/// the second.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_reaches_the_other_device() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let (a, b) = lan_pair(&code, &[], &[]).await;
    let mut ca = manager(&a).await;
    let mut cb = subscribed(&b).await;
    let a_id = a.node_id();
    // Both ends settled on what they knew before the rename.
    wait_known(&mut cb, &a_id).await;

    let mut rename = async |name: &str| {
        ca.request("devices.rename", json!({ "device_id": a_id, "name": name }))
            .await
            .expect("devices.rename with no server");
        eventually(
            async || record_of(&mut cb, &a_id).await["name"] == json!(name),
            &format!("the name {name} to reach the other device"),
        )
        .await;
        record_of(&mut cb, &a_id).await
    };

    let first = rename("Atelier").await;
    let second = rename("Atelier-2").await;

    let seq = |record: &Value| record["seq"].as_u64().expect("a seq");
    assert!(seq(&first) > 1, "over the description it replaced: {first}");
    assert!(seq(&second) > seq(&first), "{second} over {first}");
    assert!(onedevice_core::directory::verify_record(&first));
    assert!(onedevice_core::directory::verify_record(&second));
}

/// `seq` decides, not who spoke last: the fresher description wins in whichever
/// direction it travels, and the staler one overwrites nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_fresher_description_wins_whichever_way_it_travels() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let third = DeviceKey::generate();
    // A holds a description B has already superseded.
    let stale = peer_record(&third, "Old-Name", "android", &code, 2);
    let fresh = peer_record(&third, "New-Name", "android", &code, 9);
    let (a, b) = lan_pair(&code, &[stale], &[fresh]).await;
    let mut ca = subscribed(&a).await;
    let mut cb = subscribed(&b).await;

    eventually(
        async || record_of(&mut ca, &third.node_id()).await["name"] == json!("New-Name"),
        "the fresher description to reach the device holding the stale one",
    )
    .await;

    let caught_up = record_of(&mut ca, &third.node_id()).await;
    assert_eq!(caught_up["seq"], json!(9));
    assert!(onedevice_core::directory::verify_record(&caught_up));
    // And the fresher end never regressed to what the other was holding.
    let held = record_of(&mut cb, &third.node_id()).await;
    assert_eq!(held["name"], json!("New-Name"));
    assert_eq!(held["seq"], json!(9));
}

/// A revocation is the one thing that must not lag: the tombstone travels with
/// the roster, evicts the record on arrival, and keeps refusing it afterwards.
///
/// Two devices are struck off, for the same reason the rename test does it twice:
/// the first could ride a round the LAN had already triggered, the second can only
/// be carried by the nudge `devices.revoke` gives.
#[tokio::test(flavor = "multi_thread")]
async fn a_revocation_reaches_the_other_device_and_outlives_it() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let (first, second) = (DeviceKey::generate(), DeviceKey::generate());
    let records = [
        peer_record(&first, "Lost-Phone", "android", &code, 4),
        peer_record(&second, "Old-Laptop", "linux", &code, 4),
    ];
    let (a, b) = lan_pair(&code, &records, &records).await;
    let mut ca = manager(&a).await;
    let mut cb = subscribed(&b).await;
    wait_known(&mut cb, &first.node_id()).await;

    for struck in [&first, &second] {
        ca.request("devices.revoke", json!({ "device_id": struck.node_id() }))
            .await
            .expect("devices.revoke with no server");
        let removed = cb.wait_notification("device.removed").await;
        assert_eq!(removed["device_id"], json!(struck.node_id()));
        wait_unknown(&mut cb, &struck.node_id()).await;
    }

    // The tombstones are B's now, and they outlive the records coming back: the
    // store is re-seeded with both struck devices, and the restart refuses them.
    let mut store = vec![peer_record(
        b.key(),
        CORE_DEVICE_NAME,
        std::env::consts::OS,
        &code,
        1,
    )];
    store.extend_from_slice(&records);
    seed_directory(b.config_dir(), &store, 0);
    let b = b.restart().await;
    let mut cb = manager(&b).await;
    let known = known(&mut cb).await;
    assert!(
        !known.contains(&first.node_id()) && !known.contains(&second.node_id()),
        "a struck-off device does not come back from the disk: {known:?}"
    );
}

/// The case where the exchange has nothing but tombstones to carry, and the one
/// where that matters most: a device struck off with no server in sight stays
/// struck off on a sibling that HAS one.
///
/// A server-enrolled Core's records are unsigned — the server owns the names — so
/// none of them is shareable and the roster holds tombstones alone. If such a
/// roster did not travel, this half of the mechanism would be silent, and a
/// deployment that was never told about a revocation would quietly keep every
/// other device listing the struck-off device.
#[tokio::test(flavor = "multi_thread")]
async fn a_roster_of_tombstones_alone_still_travels() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    // Legitimate as far as the server and C7 are concerned: what follows is about
    // the revocation and nothing else.
    let (struck, struck_id, _conn) = attested_sibling(&server, &code, "Lost-Phone").await;
    let a = TestCore::start_lan_only_on(&server, &switchboard, &code).await;
    let b = TestCore::start_lan_only_on(&server, &switchboard, &code).await;
    let mut ca = manager(&a).await;
    let mut cb = subscribed(&b).await;
    wait_server_connected(&mut ca, true).await;
    wait_server_connected(&mut cb, true).await;
    wait_attested(&mut cb, &struck_id).await;

    // The account strikes it off on A alone, and A comes back holding only what a
    // server gave it — plus that tombstone.
    seed_revocation(a.config_dir(), &code, &struck.node_id());
    let a = a.restart().await;
    let mut ca = manager(&a).await;
    wait_server_connected(&mut ca, true).await;

    let removed = cb.wait_notification("device.removed").await;
    assert_eq!(removed["device_id"], json!(struck_id));
    wait_unknown(&mut cb, &struck_id).await;

    // And B holds it now: the server still lists the device at every reconnection,
    // and B keeps refusing it.
    let b = b.restart().await;
    let mut cb = manager(&b).await;
    wait_server_connected(&mut cb, true).await;
    assert!(
        !known(&mut cb).await.contains(&struck_id),
        "the tombstone a sibling handed over outlives what the server says"
    );
}

// ---------------------------------------------------------------------------
// Raw attested peer: forges rosters a legitimate Core never would (a compromised
// device of the account), to exercise the receiving half over the wire.
// ---------------------------------------------------------------------------

/// A device of the account whose record B already holds — so its stream is
/// accepted — but which speaks the protocol by hand.
struct RawPeer {
    key: DeviceKey,
    transport: Arc<MemoryTransport>,
    switchboard: MemorySwitchboard,
}

impl RawPeer {
    /// Its own record, as a legitimate Core would have signed it. Seeded into the
    /// target's store so `peer_in_directory` lets its stream through.
    fn record(&self, code: &str) -> Value {
        peer_record(&self.key, "Raw-Peer", std::env::consts::OS, code, 1)
    }

    /// Offers a roster and returns the answer, then waits for the target to close
    /// — which it does only AFTER absorbing, so the close is what proves the
    /// exchange is over and not merely answered.
    async fn offer(&self, target: &TestCore, roster: Value) -> Value {
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
        let mut frame = roster;
        frame["type"] = json!("dir_sync");
        let bytes = serde_json::to_vec(&frame).expect("serialize");
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .expect("frame length");
        stream.write_all(&bytes).await.expect("frame body");
        stream.flush().await.expect("flush");

        let mut len = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut len))
            .await
            .expect("an answer")
            .expect("its length");
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut body))
            .await
            .expect("the answer body")
            .expect("read");
        // The target absorbs, then closes.
        let mut sink = [0u8; 64];
        let _ = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut sink)).await;
        serde_json::from_slice(&body).expect("the answer is JSON")
    }

    /// Takes the next roster the TARGET offers us (the Core dials its peers on
    /// every round), answers `reply` verbatim, and returns the offer it made — the
    /// initiator half seen from the other side. The switchboard queues incoming
    /// streams, so a dial that happened before this call is not missed.
    async fn answer_next(&self, reply: Value) -> Value {
        let (_peer, mut stream) = tokio::time::timeout(Duration::from_secs(5), async {
            self.transport.accept().await
        })
        .await
        .expect("a round to dial us")
        .expect("a stream");
        let mut len = [0u8; 4];
        stream.read_exact(&mut len).await.expect("its offer");
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        stream.read_exact(&mut body).await.expect("the offer body");
        let bytes = serde_json::to_vec(&reply).expect("serialize");
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .expect("frame length");
        stream.write_all(&bytes).await.expect("frame body");
        stream.flush().await.expect("flush");
        // Held until the initiator is done with it (it closes after absorbing).
        let mut sink = [0u8; 64];
        let _ = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut sink)).await;
        serde_json::from_slice(&body).expect("the offer is JSON")
    }

    /// Off the fake LAN and back: the target loses its route to us and finds it
    /// again, which is what makes it run another round.
    fn reappear(&self) {
        self.switchboard.leave_lan(&self.key.node_id());
        self.switchboard.join_lan(&self.key.node_id());
    }
}

/// A Core alone in the account (no sibling Core to interfere), plus a raw peer it
/// already knows and a third device it holds a record for.
async fn core_with_raw_peer(code: &str, holding: &[Value]) -> (TestCore, RawPeer) {
    let switchboard = MemorySwitchboard::new();
    // Registered under its real node_id, on the LAN like the Core it talks to.
    let key = DeviceKey::generate();
    let transport = switchboard.endpoint(key.node_id(), None);
    switchboard.join_lan(&key.node_id());
    let raw = RawPeer {
        key,
        transport,
        switchboard: switchboard.clone(),
    };
    let mut store = vec![raw.record(code)];
    store.extend_from_slice(holding);
    let core =
        TestCore::start_in_account_on(code, &switchboard, DeviceKey::generate(), &store).await;
    (core, raw)
}

/// The receiving half is fail-closed on every count: a record the account never
/// attested, one nothing signed, one rewritten in transit, and a tombstone the
/// account never signed. None of them changes anything — and the answer still
/// comes back, so a hostile roster is refused, not fatal.
#[tokio::test(flavor = "multi_thread")]
async fn a_roster_it_cannot_prove_teaches_the_receiver_nothing() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let other_code = onedevice_core::account_key::generate_recovery_code();
    let third = DeviceKey::generate();
    let held = peer_record(&third, "Phone", "android", &code, 4);
    let (core, raw) = core_with_raw_peer(&code, &[held]).await;
    let mut c = manager(&core).await;
    let before = known(&mut c).await;

    let intruder = DeviceKey::generate();
    let mut rewritten = peer_record(&third, "Phone", "android", &code, 9);
    rewritten["name"] = json!("Not-What-It-Signed");
    let unsigned = json!({
        "device_id": "d_1", "node_id": intruder.node_id(), "name": "Hearsay",
        "platform": "linux",
        "attestation": onedevice_core::account_key::attest(
            &onedevice_core::account_key::account_key_from_code(&code).expect("code"),
            &intruder.node_id(),
        ),
    });
    let answer = raw
        .offer(
            &core,
            json!({
                "records": [
                    // Attested under another account entirely.
                    peer_record(&intruder, "Outsider", "linux", &other_code, 1),
                    unsigned,
                    rewritten,
                    {},
                ],
                // A tombstone against a device it holds, signed by nobody.
                "revoked": { third.node_id(): "de".repeat(64) },
            }),
        )
        .await;

    assert_eq!(answer["type"], json!("dir_roster"));
    assert!(
        answer["records"].as_array().is_some_and(|r| !r.is_empty()),
        "the answer carries what we can prove: {answer}"
    );
    assert_eq!(known(&mut c).await, before, "and we learned nothing");
    assert_eq!(
        record_of(&mut c, &third.node_id()).await["name"],
        json!("Phone"),
        "the description we held is the one that device signed"
    );
}

/// A relayer cannot plant a route. The hints are covered by the device's own
/// signature precisely so that whoever carries a record cannot rewrite where
/// it points: a planted address would send every future dial (and its
/// authenticated handshake) to a machine of the relayer's choosing.
#[tokio::test(flavor = "multi_thread")]
async fn a_relayer_cannot_plant_a_route_on_a_record_it_carries() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let third = DeviceKey::generate();
    let held = peer_record_reaching(
        &third,
        "Phone",
        "android",
        &code,
        4,
        &onedevice_core::Reach {
            addrs: vec!["10.8.0.2:41641".to_string()],
            relay_hint: None,
        },
    );
    let (core, raw) = core_with_raw_peer(&code, &[held]).await;
    let mut c = manager(&core).await;

    // The genuine fresher description, with its addresses rewritten in
    // transit, and a second copy whose relay hint was planted instead.
    let mut rerouted = peer_record_reaching(
        &third,
        "Phone",
        "android",
        &code,
        9,
        &onedevice_core::Reach {
            addrs: vec!["10.8.0.2:41641".to_string()],
            relay_hint: None,
        },
    );
    rerouted["addrs"] = json!(["203.0.113.66:41641"]);
    let mut rerelayed = rerouted.clone();
    rerelayed["addrs"] = json!(["10.8.0.2:41641"]);
    rerelayed["relay_hint"] = json!("https://liar.example");
    let answer = raw
        .offer(
            &core,
            json!({ "records": [rerouted, rerelayed], "revoked": {} }),
        )
        .await;

    assert_eq!(answer["type"], json!("dir_roster"));
    let kept = record_of(&mut c, &third.node_id()).await;
    assert_eq!(kept["seq"], json!(4), "the tampered supersede never landed");
    assert_eq!(kept["addrs"], json!(["10.8.0.2:41641"]));
    assert_eq!(kept["relay_hint"], json!(null));
}

/// An exchange that teaches nothing costs nothing: not a notification, not a disk
/// write. Without this the store's timestamp would move on every round, quietly
/// extending the staleness bound of records nobody re-checked.
#[tokio::test(flavor = "multi_thread")]
async fn an_exchange_that_teaches_nothing_touches_nothing() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let (core, raw) = core_with_raw_peer(&code, &[]).await;
    let mut c = subscribed(&core).await;
    let store = core.config_dir().join("directory.json");
    // Whatever the startup did, it is done: the raw peer is known.
    wait_known(&mut c, &raw.key.node_id()).await;
    c.drain().await;
    let before = std::fs::read(&store).expect("the store");

    // Its own record, which we already hold, at the same `seq`.
    let answer = raw
        .offer(&core, json!({ "records": [raw.record(&code)] }))
        .await;

    assert_eq!(answer["type"], json!("dir_roster"));
    assert_eq!(
        std::fs::read(&store).expect("the store"),
        before,
        "the store was not rewritten"
    );
    c.assert_silent().await;
}

/// The answer is what the responder HELD, not what it has just been told. Built
/// the other way round, every exchange would hand each side a second copy of its
/// own roster back — for nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_answer_is_what_the_responder_held_not_what_it_just_learned() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let (core, raw) = core_with_raw_peer(&code, &[]).await;
    let mut c = manager(&core).await;
    let third = DeviceKey::generate();

    let answer = raw
        .offer(
            &core,
            json!({ "records": [peer_record(&third, "Phone", "android", &code, 4)] }),
        )
        .await;

    let echoed = answer["records"]
        .as_array()
        .expect("records")
        .iter()
        .any(|record| record["node_id"] == json!(third.node_id()));
    assert!(!echoed, "our own roster came back at us: {answer}");
    // And it WAS taken in — the answer being silent about it is not the Core
    // ignoring it.
    wait_known(&mut c, &third.node_id()).await;
}

/// The initiator OFFERS, it does not merely ask. Both ends of a live pair dial
/// each other, so a broken offer would still converge through the answers alone —
/// and half the mechanism would be dead without a single test noticing. Seen from
/// the peer the Core dialled, the offer must carry what the Core knows.
#[tokio::test(flavor = "multi_thread")]
async fn the_offer_carries_what_this_device_knows() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let third = DeviceKey::generate();
    let (core, raw) =
        core_with_raw_peer(&code, &[peer_record(&third, "Phone", "android", &code, 4)]).await;

    // The Core dials on its startup round; we answer with nothing to teach it.
    let offer = raw
        .answer_next(json!({ "type": "dir_roster", "records": [], "revoked": {} }))
        .await;

    assert_eq!(offer["type"], json!("dir_sync"));
    let offered: Vec<&str> = offer["records"]
        .as_array()
        .expect("records")
        .iter()
        .map(|record| record["node_id"].as_str().expect("node_id"))
        .collect();
    assert!(
        offered.contains(&core.node_id().as_str()),
        "its own description: {offer}"
    );
    assert!(
        offered.contains(&third.node_id().as_str()),
        "and the sibling only it knows — which is what makes us learn: {offer}"
    );
    for record in offer["records"].as_array().expect("records") {
        assert!(
            onedevice_core::directory::verify_record(record),
            "every record offered is one we could check: {record}"
        );
    }
}

/// What a sibling taught us reaches the disk, and does NOT reset the store's
/// freshness stamp. That stamp is a bound against the SERVER — a roster carries the
/// account's tombstones but not a server-side `devices.revoke`, which mints none —
/// so if an exchange counted as a refresh, two devices left talking to each other
/// in a room could hold the bound open between themselves and keep vouching for a
/// device the server revoked.
#[tokio::test(flavor = "multi_thread")]
async fn what_a_sibling_taught_us_does_not_refresh_the_store() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let (core, raw) = core_with_raw_peer(&code, &[]).await;
    let mut c = manager(&core).await;
    let store = core.config_dir().join("directory.json");
    // A store stamped well in the past, so a refresh would be unmistakable. (A
    // serverless Core does not rewrite it at startup — the boot self-record is
    // deliberately not persisted — so this stamp is what the exchange will find.)
    let stamp = 1_000_000_u64;
    let mut on_disk: Value =
        serde_json::from_slice(&std::fs::read(&store).expect("the store")).expect("JSON");
    on_disk["saved_at"] = json!(stamp);
    std::fs::write(&store, on_disk.to_string()).expect("re-stamp the store");

    let third = DeviceKey::generate();
    raw.offer(
        &core,
        json!({ "records": [peer_record(&third, "Phone", "android", &code, 4)] }),
    )
    .await;
    wait_known(&mut c, &third.node_id()).await;

    let after: Value =
        serde_json::from_slice(&std::fs::read(&store).expect("the store")).expect("JSON");
    assert!(
        after["devices"].get(third.node_id()).is_some(),
        "what it learned reached the disk: {after}"
    );
    assert_eq!(
        after["saved_at"],
        json!(stamp),
        "and the freshness stamp did not move: {after}"
    );
}

/// A tombstone for a device this Core has never held changes nothing to see — and
/// still has to reach the disk, or the record walks in the moment it shows up.
#[tokio::test(flavor = "multi_thread")]
async fn a_tombstone_for_an_unknown_device_is_kept_for_when_it_shows_up() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let (core, raw) = core_with_raw_peer(&code, &[]).await;
    let mut c = manager(&core).await;
    let stranger = DeviceKey::generate();
    let ak = onedevice_core::account_key::account_key_from_code(&code).expect("code");

    let answer = raw
        .offer(
            &core,
            json!({
                "records": [],
                "revoked": {
                    stranger.node_id():
                        onedevice_core::account_key::revoke(&ak, &stranger.node_id()),
                },
            }),
        )
        .await;

    assert_eq!(answer["type"], json!("dir_roster"));
    // Nothing left the directory — there was nothing to leave.
    assert!(!known(&mut c).await.contains(&stranger.node_id()));
    // But the tombstone is on disk, so the record does not walk in later: it is
    // seeded into the store, and the restart refuses it.
    let mut store = vec![peer_record(
        core.key(),
        CORE_DEVICE_NAME,
        std::env::consts::OS,
        &code,
        1,
    )];
    store.push(peer_record(&stranger, "Latecomer", "linux", &code, 4));
    seed_directory(core.config_dir(), &store, 0);
    let core = core.restart().await;
    let mut c = manager(&core).await;
    assert!(
        !known(&mut c).await.contains(&stranger.node_id()),
        "the account struck it off before this Core ever held its record"
    );
}

/// The initiating half validates too: an answer that is not a roster teaches
/// nothing, and the round it belongs to survives — the next one converges as
/// usual.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_is_not_a_roster_teaches_nothing() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let (core, raw) = core_with_raw_peer(&code, &[]).await;
    let mut c = manager(&core).await;
    let sneak = DeviceKey::generate();
    let legitimate = DeviceKey::generate();

    // The Core dials on its startup round: answer with a well-formed roster
    // wearing the wrong frame type.
    raw.answer_next(json!({
        "type": "clip_ack",
        "records": [peer_record(&sneak, "Sneak", "linux", &code, 1)],
        "revoked": {},
    }))
    .await;

    // Another round, answered properly this time. Rounds are sequential, so the
    // Core knowing the second device proves it is done with the first answer.
    raw.reappear();
    raw.answer_next(json!({
        "type": "dir_roster",
        "records": [peer_record(&legitimate, "Phone", "android", &code, 4)],
        "revoked": {},
    }))
    .await;
    wait_known(&mut c, &legitimate.node_id()).await;

    assert!(
        !known(&mut c).await.contains(&sneak.node_id()),
        "a record that arrived under the wrong frame type was refused"
    );
}
