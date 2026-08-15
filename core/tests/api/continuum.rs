// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The continuum: one account, half of it on a server, half of it not.
//!
//! Before this brick the two halves could not see each other — a record a server
//! minted carried no self-signature, so it never left the device that held it,
//! and a `1D2` code was refused outright on a device that answered to a server.
//! What ties the halves together now is that every device stands behind its own
//! description WHEREVER it was named: the server keeps naming its half
//! (`devices.rename` still goes through it), and the named device countersigns —
//! the name is the server's word, the signature is nobody's but its own. From
//! there everything is the serverless machinery, unchanged: signed-and-attested
//! records travel by `dirsync`, tombstones outrank whatever a server says, and
//! the account is the union the ACCOUNT KEY defines, not the subset a deployment
//! happens to list.
//!
//! What is checked here is the seams, end to end: a server-enrolled Core and a
//! serverless-only sibling seeing each other while the server never learns the
//! sibling exists; the server carrying the signed description (opaquely, like
//! the attestation) so its half can relay devices it never met in person; a
//! rename through the server reaching the serverless half re-signed; a
//! revocation through the server minting the account's own tombstone besides
//! the server's strike; and a `1D2` pairing sponsored by a device that answers
//! to a server — the joiner joining the account, not the deployment.

use onedevice_test_support::memory_transport::MemorySwitchboard;
use serde_json::{Value, json};

use crate::support::*;

/// A GUI-shaped component: manages the session and the devices, sees both.
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

/// A manager watching the topics the continuum shows up on.
async fn watching(core: &TestCore) -> TestComponent {
    let mut c = manager(core).await;
    c.request(
        "events.subscribe",
        json!({ "topics": ["devices", "session", "pairing"] }),
    )
    .await
    .expect("events.subscribe");
    c
}

/// The record a Core currently serves for `node_id`, if any.
async fn record_of(c: &mut TestComponent, node_id: &str) -> Option<Value> {
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    list.as_array()
        .expect("a list")
        .iter()
        .find(|d| d["node_id"] == json!(node_id))
        .cloned()
}

/// The two halves of one continuum scenario: a Core enrolled on the server AND
/// on the LAN (holding the account key), and a serverless-only Core on the same
/// LAN — each seeded with the other's record, which is what the pairing brick
/// would have arranged (`lanpair.rs` proves that part on its own).
async fn two_halves(
    server: &TestServer,
    switchboard: &MemorySwitchboard,
    code: &str,
) -> (TestCore, TestCore) {
    let enrolled_key = DeviceKey::generate();
    let nomad_key = DeviceKey::generate();
    // Each holds the other's record exactly as that device would have signed it —
    // the same description the device seeds for itself, so the two views agree.
    let enrolled_record = peer_record(
        &enrolled_key,
        CORE_DEVICE_NAME,
        std::env::consts::OS,
        code,
        1,
    );
    let nomad_record = peer_record(&nomad_key, CORE_DEVICE_NAME, std::env::consts::OS, code, 1);
    let enrolled = TestCore::start_enrolled_lan_only_holding(
        server,
        switchboard,
        code,
        enrolled_key,
        &[nomad_record],
    )
    .await;
    let nomad =
        TestCore::start_in_account_on(code, switchboard, nomad_key, &[enrolled_record]).await;
    (enrolled, nomad)
}

/// The server's directory, read by a freshly enrolled onlooker — what a device
/// of the server's half is served when it arrives.
async fn server_directory(server: &TestServer) -> Vec<Value> {
    let key = DeviceKey::generate();
    let mut conn = server.connect_direct().await;
    let device_id = enroll_key(&mut conn, &server.oidc, &key, TEST_SUB, "Onlooker", "linux").await;
    authenticate(&mut conn, &key, &device_id).await;
    let listed = conn
        .request("devices.list", json!({}))
        .await
        .expect("server-side devices.list");
    listed.as_array().expect("a list").clone()
}

/// The flagship: the two halves of the account see each other, each through its
/// own channel — and the server never learns the serverless device exists.
#[tokio::test(flavor = "multi_thread")]
async fn the_two_halves_of_the_account_see_each_other() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let (enrolled, nomad) = two_halves(&server, &switchboard, &code).await;
    let mut ec = watching(&enrolled).await;
    let mut nc = watching(&nomad).await;
    wait_server_connected(&mut ec, true).await;

    // The enrolled Core's own record — as the SERVER serves it — carries its
    // countersigned description: published like the attestation, stored blind,
    // and what lets any device of the server's half relay it onward.
    let listed = server_directory(&server).await;
    let own = listed
        .iter()
        .find(|d| d["node_id"] == json!(enrolled.key().node_id()))
        .expect("the enrolled Core in the server's directory");
    assert!(
        onedevice_core::directory::verify_record(own),
        "the server carries the countersigned description: {own}"
    );
    // And the serverless device is nowhere in it.
    assert!(
        listed
            .iter()
            .all(|d| d["node_id"] != json!(nomad.key().node_id())),
        "the server never learns the serverless half exists: {listed:?}"
    );

    // The serverless sibling ends up holding the enrolled Core's LIVE
    // description — the countersigned one (seq above the seeded 1), learned
    // over dirsync — and the enrolled Core still serves the sibling.
    eventually(
        async || {
            record_of(&mut nc, &enrolled.key().node_id())
                .await
                .is_some_and(|record| {
                    onedevice_core::directory::verify_record(&record)
                        && record["seq"].as_u64().is_some_and(|seq| seq > 1)
                })
        },
        "the enrolled Core's countersigned record to reach the serverless sibling",
    )
    .await;
    let nomad_seen = record_of(&mut ec, &nomad.key().node_id())
        .await
        .expect("the serverless sibling in the enrolled Core's directory");
    assert!(onedevice_core::directory::verify_record(&nomad_seen));

    // And the sibling SURVIVES the server: a reconnection replays the snapshot,
    // which now merges instead of replacing.
    server.cut();
    wait_server_connected(&mut ec, false).await;
    server.restore();
    wait_server_connected(&mut ec, true).await;
    assert!(
        record_of(&mut ec, &nomad.key().node_id()).await.is_some(),
        "the account's half must survive the server's snapshot"
    );
}

/// The server's half relays a device it never met in person: C holds A's record
/// only through the SERVER (attestation and signed description carried blind),
/// and hands it to a serverless sibling D that has never seen A — gossip
/// transitivity across the halves.
#[tokio::test(flavor = "multi_thread")]
async fn the_servers_half_relays_a_sibling_it_never_met_in_person() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();

    // A: enrolled, with a relay — off the LAN, so D can never hear it directly.
    let a = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    // C: enrolled AND on the LAN; D: serverless, knowing only C.
    let c_key = DeviceKey::generate();
    let d_key = DeviceKey::generate();
    let c_record = peer_record(&c_key, CORE_DEVICE_NAME, std::env::consts::OS, &code, 1);
    let d_record = peer_record(&d_key, CORE_DEVICE_NAME, std::env::consts::OS, &code, 1);
    let c =
        TestCore::start_enrolled_lan_only_holding(&server, &switchboard, &code, c_key, &[d_record])
            .await;
    let d = TestCore::start_in_account_on(&code, &switchboard, d_key, &[c_record]).await;

    let mut ac = manager(&a).await;
    let mut cc = manager(&c).await;
    let mut dc = manager(&d).await;
    wait_server_connected(&mut ac, true).await;
    wait_server_connected(&mut cc, true).await;

    // C holds A through the server, provably: signed by A, attested, relayable.
    eventually(
        async || {
            record_of(&mut cc, &a.key().node_id())
                .await
                .is_some_and(|record| onedevice_core::directory::verify_record(&record))
        },
        "A's record, server-carried, to prove itself on C",
    )
    .await;

    // D learns A from C — a device D has never met, on a server D cannot reach.
    eventually(
        async || {
            record_of(&mut dc, &a.key().node_id())
                .await
                .is_some_and(|record| onedevice_core::directory::verify_record(&record))
        },
        "A's record to reach D through C",
    )
    .await;
}

/// A rename through the server reaches the serverless half: the renamed device
/// countersigns the server's word and the account carries it — nobody else
/// could, since nobody else may sign that device's description.
#[tokio::test(flavor = "multi_thread")]
async fn renaming_through_the_server_reaches_the_serverless_half() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let (enrolled, nomad) = two_halves(&server, &switchboard, &code).await;
    let mut ec = manager(&enrolled).await;
    let mut nc = watching(&nomad).await;
    wait_server_connected(&mut ec, true).await;
    // The halves have met (the flagship test owns the full assertions).
    eventually(
        async || {
            record_of(&mut nc, &enrolled.key().node_id())
                .await
                .is_some_and(|record| record["seq"].as_u64().is_some_and(|seq| seq > 1))
        },
        "the halves to have met",
    )
    .await;

    ec.request(
        "devices.rename",
        json!({ "device_id": enrolled.device_id(), "name": "Atelier" }),
    )
    .await
    .expect("devices.rename through the server");

    eventually(
        async || {
            record_of(&mut nc, &enrolled.key().node_id())
                .await
                .is_some_and(|record| {
                    record["name"] == json!("Atelier")
                        && onedevice_core::directory::verify_record(&record)
                })
        },
        "the server's rename, countersigned, to reach the serverless half",
    )
    .await;
    // The server dropped the stale signature at the rename; the fresh one was
    // republished to it, so ITS record proves the new name too.
    let (_key, _id, mut onlooker) = attested_sibling(&server, &code, "Onlooker").await;
    eventually(
        async || {
            let list = onlooker
                .request("devices.list", json!({}))
                .await
                .expect("server-side devices.list");
            list.as_array().expect("a list").iter().any(|d| {
                d["node_id"] == json!(enrolled.key().node_id())
                    && d["name"] == json!("Atelier")
                    && onedevice_core::directory::verify_record(d)
            })
        },
        "the republished signature to reach the server",
    )
    .await;
}

/// A rename that happened while this device was AWAY: the reconnection's
/// snapshot brings a name its held signature no longer covers. The proof is not
/// grafted (a signature must cover what the record says), and the device
/// countersigns the server's word on the spot — then publishes it, so both
/// halves hear a name nobody could sign but itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_that_happened_while_away_is_countersigned_at_reconnection() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let (enrolled, nomad) = two_halves(&server, &switchboard, &code).await;
    let mut ec = manager(&enrolled).await;
    let mut nc = manager(&nomad).await;
    wait_server_connected(&mut ec, true).await;
    // Another device of the account, to do the renaming while E is away. Its
    // connection is direct: the cut below severs only the Cores.
    let (_renamer_key, _renamer_id, mut renamer) =
        attested_sibling(&server, &code, "Renamer").await;

    server.cut();
    wait_server_connected(&mut ec, false).await;
    renamer
        .request(
            "devices.rename",
            json!({ "device_id": enrolled.device_id(), "name": "Away-Name" }),
        )
        .await
        .expect("rename in absentia");
    server.restore();
    wait_server_connected(&mut ec, true).await;

    // The server's directory serves the new name WITH a signature that proves
    // it — the renamed device's own, minted at reconnection.
    eventually(
        async || {
            let list = renamer
                .request("devices.list", json!({}))
                .await
                .expect("server-side devices.list");
            list.as_array().expect("a list").iter().any(|d| {
                d["node_id"] == json!(enrolled.key().node_id())
                    && d["name"] == json!("Away-Name")
                    && onedevice_core::directory::verify_record(d)
            })
        },
        "the countersigned rename to reach the server",
    )
    .await;
    // And the serverless half hears it too: the reconnection nudges the rosters.
    eventually(
        async || {
            record_of(&mut nc, &enrolled.key().node_id())
                .await
                .is_some_and(|record| {
                    record["name"] == json!("Away-Name")
                        && onedevice_core::directory::verify_record(&record)
                })
        },
        "the countersigned rename to reach the serverless half",
    )
    .await;
}

/// A device the ACCOUNT taught this Core first (keyed by its `node_id`) is
/// re-keyed, not duplicated, when the server later names it: one device, one
/// entry — and the old label leaves as a `device.removed` before the record
/// arrives under the server's.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_the_account_knew_first_is_rekeyed_when_the_server_names_it() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let late_key = DeviceKey::generate();
    let late_record = peer_record(&late_key, "Latecomer", std::env::consts::OS, &code, 1);
    let enrolled = TestCore::start_enrolled_lan_only_holding(
        &server,
        &switchboard,
        &code,
        DeviceKey::generate(),
        &[late_record],
    )
    .await;
    let mut ec = watching(&enrolled).await;
    wait_server_connected(&mut ec, true).await;
    let late_node = late_key.node_id();
    let first = record_of(&mut ec, &late_node)
        .await
        .expect("known first from the account");
    assert_eq!(first["device_id"], json!(late_node), "self-minted label");

    // The server names it (an enrollment): the record moves under the server's
    // label, the old one leaving first.
    let mut conn = server.connect_direct().await;
    let late_id = enroll_key(
        &mut conn,
        &server.oidc,
        &late_key,
        TEST_SUB,
        "Latecomer",
        std::env::consts::OS,
    )
    .await;
    let removed = ec.wait_notification("device.removed").await;
    assert_eq!(removed["device_id"], json!(late_node));
    eventually(
        async || {
            record_of(&mut ec, &late_node)
                .await
                .is_some_and(|record| record["device_id"] == json!(late_id))
        },
        "the record under the server's label",
    )
    .await;
    let list = ec
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(
        list.as_array()
            .expect("a list")
            .iter()
            .filter(|d| d["node_id"] == json!(late_node))
            .count(),
        1,
        "one device, one entry: {list}"
    );
}

/// A self-revocation through the server stays what it always was — the session
/// dies at the server's word — and mints NO tombstone: signing against one's own
/// `node_id` is `CANNOT_REVOKE_SELF` everywhere else, and a server's yes does
/// not change whose signature it would be.
#[tokio::test(flavor = "multi_thread")]
async fn a_self_revocation_through_the_server_mints_no_tombstone() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let enrolled = TestCore::start_enrolled_lan_only_holding(
        &server,
        &switchboard,
        &code,
        DeviceKey::generate(),
        &[],
    )
    .await;
    let mut ec = watching(&enrolled).await;
    wait_server_connected(&mut ec, true).await;

    // No refresh token in the seeded session: the browser settles it.
    let r = ec
        .request(
            "devices.revoke",
            json!({ "device_id": enrolled.device_id() }),
        )
        .await
        .expect("devices.revoke of oneself");
    assert_eq!(r["status"], "reauth_required");
    let page = browse(r["auth_url"].as_str().expect("auth_url"))
        .await
        .expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);
    // The server closes the session (DEVICE_REVOKED, `drop_session`).
    wait_server_connected(&mut ec, false).await;

    let revoked =
        std::fs::read_to_string(enrolled.config_dir().join("revoked.json")).unwrap_or_default();
    assert!(
        !revoked.contains(&enrolled.key().node_id()),
        "a tombstone against one's own node_id: {revoked}"
    );
}

/// A revocation through the server also mints the account's own tombstone (the
/// device holds the account key): the serverless half hears it and strikes the
/// device too — a server-side strike alone would never have reached it.
#[tokio::test(flavor = "multi_thread")]
async fn revoking_through_the_server_strikes_the_serverless_half_too() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let (enrolled, nomad) = two_halves(&server, &switchboard, &code).await;
    let mut ec = manager(&enrolled).await;
    let mut nc = watching(&nomad).await;
    wait_server_connected(&mut ec, true).await;

    // The victim: enrolled and online on the server, attested, publishing its
    // signed description — everything a legitimate sibling of the server's half
    // has. Its record reaches the serverless side by gossip.
    let (victim_key, victim_id, mut victim_conn) =
        attested_sibling(&server, &code, "PC-Victim").await;
    let victim_node = victim_key.node_id();
    let seq = 3u64;
    victim_conn
        .request(
            "presence.update",
            json!({
                "seq": seq,
                "self_sig": victim_key.sign(&onedevice_core::directory::record_message(
                    &victim_node,
                    "PC-Victim",
                    std::env::consts::OS,
                    seq,
                    &onedevice_core::Reach::default(),
                )),
            }),
        )
        .await
        .expect("the victim publishes its signed description");
    eventually(
        async || record_of(&mut nc, &victim_node).await.is_some(),
        "the victim's record to reach the serverless half",
    )
    .await;

    // The revocation goes through the browser (the seeded session has no
    // refresh token): the re-auth path, which must mint the tombstone too.
    let r = ec
        .request("devices.revoke", json!({ "device_id": victim_id }))
        .await
        .expect("devices.revoke");
    assert_eq!(r["status"], "reauth_required");
    let page = browse(r["auth_url"].as_str().expect("auth_url"))
        .await
        .expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);

    // The server's half saw the strike…
    eventually(
        async || record_of(&mut ec, &victim_node).await.is_none(),
        "the victim to leave the enrolled Core's directory",
    )
    .await;
    // …the revoker holds the tombstone durably…
    let revoked =
        std::fs::read_to_string(enrolled.config_dir().join("revoked.json")).expect("revoked.json");
    assert!(revoked.contains(&victim_node), "{revoked}");
    // …and so does the serverless half, which no server speaks to: the tombstone
    // travelled.
    eventually(
        async || record_of(&mut nc, &victim_node).await.is_none(),
        "the tombstone to reach the serverless half",
    )
    .await;
    let revoked = std::fs::read_to_string(nomad.config_dir().join("revoked.json"))
        .expect("the serverless sibling holds the tombstone");
    let revoked: Value = serde_json::from_str(&revoked).expect("readable revoked.json");
    let tombstone = revoked["revoked"][&victim_node]
        .as_str()
        .expect("the victim's tombstone");
    let ak_pub = onedevice_core::account_key::public_hex(
        &onedevice_core::account_key::account_key_from_code(&code).expect("valid code"),
    );
    assert!(
        onedevice_core::account_key::verify_revocation(&ak_pub, &victim_node, tombstone),
        "and it is the account's own signature"
    );

    // The re-auth stashed a refresh token: the next revocation is browserless —
    // and mints its tombstone all the same (the fresh-token path).
    let (victim2_key, victim2_id, mut victim2_conn) =
        attested_sibling(&server, &code, "PC-Victim-2").await;
    let victim2_node = victim2_key.node_id();
    let seq2 = 3u64;
    victim2_conn
        .request(
            "presence.update",
            json!({
                "seq": seq2,
                "self_sig": victim2_key.sign(&onedevice_core::directory::record_message(
                    &victim2_node,
                    "PC-Victim-2",
                    std::env::consts::OS,
                    seq2,
                    &onedevice_core::Reach::default(),
                )),
            }),
        )
        .await
        .expect("the second victim publishes its signed description");
    eventually(
        async || record_of(&mut nc, &victim2_node).await.is_some(),
        "the second victim's record to reach the serverless half",
    )
    .await;
    let r = ec
        .request("devices.revoke", json!({ "device_id": victim2_id }))
        .await
        .expect("devices.revoke, browserless");
    assert_eq!(r["status"], "done");
    eventually(
        async || record_of(&mut nc, &victim2_node).await.is_none(),
        "the second tombstone to reach the serverless half",
    )
    .await;
}

/// Revoking a device the server never named (its label IS its `node_id`) goes by
/// the account's signature alone: the server is not asked — it would only answer
/// DEVICE_UNKNOWN — and the strike works exactly as it does with no server.
#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_device_the_server_never_named_goes_by_the_account() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let (enrolled, nomad) = two_halves(&server, &switchboard, &code).await;
    let mut ec = watching(&enrolled).await;
    wait_server_connected(&mut ec, true).await;
    let nomad_node = nomad.key().node_id();

    let r = ec
        .request("devices.revoke", json!({ "device_id": nomad_node }))
        .await
        .expect("devices.revoke of a serverless-only sibling");
    assert_eq!(
        r["status"], "done",
        "no browser, no server: the account key"
    );

    let removed = ec.wait_notification("device.removed").await;
    assert_eq!(removed["device_id"], json!(nomad_node));
    assert!(record_of(&mut ec, &nomad_node).await.is_none());
    // The tombstone is on disk, the account's own word.
    let revoked =
        std::fs::read_to_string(enrolled.config_dir().join("revoked.json")).expect("revoked.json");
    assert!(revoked.contains(&nomad_node), "{revoked}");
}

/// A device that answers to a server sponsors over the local network: the `1D2`
/// refusal (`PAIRING_VIA_SERVER`) fell with the continuum. The joiner joins the
/// ACCOUNT — key, roster, directory — and the deployment simply never lists it.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_with_a_server_sponsors_over_the_local_network() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let enrolled = TestCore::start_enrolled_lan_only_holding(
        &server,
        &switchboard,
        &code,
        DeviceKey::generate(),
        &[],
    )
    .await;
    let fresh = TestCore::start_fresh_on(&switchboard, DeviceKey::generate()).await;
    let mut ec = watching(&enrolled).await;
    let mut fc = watching(&fresh).await;
    wait_server_connected(&mut ec, true).await;

    // The fresh device shows the code (it is the one with no server and no
    // account); the enrolled device scans it — and sponsors.
    let offer = fc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer on the fresh device");
    let code_shown = offer["code"].as_str().expect("a code").to_string();
    assert!(code_shown.starts_with("1D2:"), "{code_shown}");
    let claim = ec
        .request("pairing.accept", json!({ "code": code_shown }))
        .await
        .expect("a device with a server may scan a 1D2 code now");
    assert_eq!(claim["role"], json!("sponsor"), "it holds the account key");
    assert_eq!(
        claim["device"]["node_id"],
        json!(fresh.node_id()),
        "and is shown whom it is vouching for"
    );

    let pairing_id = claim["pairing_id"].as_str().expect("pairing_id");
    ec.request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect("pairing.confirm");
    fc.wait_notification("pairing.completed").await;

    // The joiner is in the ACCOUNT…
    let status = fc
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true));
    assert_eq!(status["holds_key"], json!(true));
    // …the two of them know each other…
    eventually(
        async || {
            record_of(&mut fc, &enrolled.key().node_id())
                .await
                .is_some()
        },
        "the joiner to know its sponsor",
    )
    .await;
    eventually(
        async || record_of(&mut ec, &fresh.node_id()).await.is_some(),
        "the sponsor to know the joiner",
    )
    .await;
    // …and the deployment never heard of it.
    let listed = server_directory(&server).await;
    assert!(
        listed
            .iter()
            .all(|d| d["node_id"] != json!(fresh.node_id())),
        "the joiner joined the account, not the deployment: {listed:?}"
    );
}

/// The reach rides the server like the rest of the signed description. A
/// device whose transport observes fresh addresses re-signs and republishes
/// (`presence.update`, opaque to the server); a sibling of the server's half
/// receives a record that carries the hints AND still proves itself: one
/// signature covers the name, the `seq` and the reach, so the server cannot
/// redistribute the proof without the hints, nor the reverse.
#[tokio::test(flavor = "multi_thread")]
async fn the_server_carries_the_reach_to_its_own_half() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let a = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let b = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let mut ca = manager(&a).await;
    let mut cb = manager(&b).await;
    wait_server_connected(&mut ca, true).await;
    wait_server_connected(&mut cb, true).await;
    wait_attested(&mut cb, a.device_id()).await;

    // A's transport observes where it can be dialed (a VPN came up, say).
    switchboard.declare_reach(
        &a.node_id(),
        onedevice_core::Reach {
            addrs: vec!["10.8.0.7:41641".to_string()],
            relay_hint: None,
        },
    );

    eventually(
        async || {
            record_of(&mut cb, &a.node_id())
                .await
                .is_some_and(|d| d["addrs"] == json!(["10.8.0.7:41641"]))
        },
        "A's signed addresses to reach B through the server",
    )
    .await;
    let learned = record_of(&mut cb, &a.node_id())
        .await
        .expect("A's record on B");
    assert!(
        onedevice_core::directory::verify_record(&learned),
        "the proof arrived WITH the hints it covers: {learned}"
    );
    // And the SERVER holds them: both Cores share a switchboard, so the
    // gossip could have carried the reach on its own; the server's own copy
    // is what pins the presence.update hop this test is about. Converging,
    // because the republication is fire-and-forget beside the gossip nudge.
    eventually(
        async || {
            server_directory(&server).await.iter().any(|d| {
                d["node_id"] == json!(a.node_id()) && d["addrs"] == json!(["10.8.0.7:41641"])
            })
        },
        "the reach to be published to the server, not only gossiped",
    )
    .await;
}
