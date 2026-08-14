// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Being struck off, heard and obeyed: the account's tombstone naming THIS
//! device makes it leave the account (serverless, building block 5).
//!
//! The delivery is the part worth reading twice. A tombstone cannot reach the
//! device it names by gossip: absorbing it is what evicts that device from every
//! sibling's directory, so the siblings refuse the very streams that could have
//! carried it — and stop dialling it. Enforcing the revocation is exactly what
//! blocks its delivery. What remains is the struck-off device's own dial: the
//! data plane answers it with a one-entry roster — its tombstone — and the same
//! absorb that reads rosters is the one that obeys it.
//!
//! Obeying means leaving whole: the trust root, the account key, the session,
//! the directory, the tombstones, and `device.key` itself — a revocation is
//! permanent, so the only way back is the fresh identity the next startup mints.

use onedevice_test_support::memory_transport::MemorySwitchboard;
use serde_json::{Value, json};

use crate::lanpair::Stranger;
use crate::support::*;

/// A GUI-shaped component: manages the account and reads the directory.
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

/// A manager watching every topic a leave shows up on.
async fn watching(core: &TestCore) -> TestComponent {
    let mut c = manager(core).await;
    c.request(
        "events.subscribe",
        json!({ "topics": ["session", "devices", "pairing"] }),
    )
    .await
    .expect("events.subscribe");
    c
}

async fn account_status(c: &mut TestComponent) -> Value {
    c.request("account.status", json!({}))
        .await
        .expect("account.status")
}

/// Two serverless Cores of one account on the same LAN, each holding the other's
/// record — the state a pairing leaves behind, and the one a revocation tears.
async fn account_pair(code: &str, switchboard: &MemorySwitchboard) -> (TestCore, TestCore) {
    let a_key = DeviceKey::generate();
    let c_key = DeviceKey::generate();
    let describe =
        |key: &DeviceKey| peer_record(key, CORE_DEVICE_NAME, std::env::consts::OS, code, 1);
    let (a_record, c_record) = (describe(&a_key), describe(&c_key));
    let a = TestCore::start_in_account_on(code, switchboard, a_key, &[c_record]).await;
    let c = TestCore::start_in_account_on(code, switchboard, c_key, &[a_record]).await;
    (a, c)
}

/// The whole path, on two live Cores: A strikes C off, C hears it on its next
/// dial and leaves. Returns C after its `account.left`, with the component that
/// watched it happen and the identity C had while it was a member.
async fn struck_and_left(code: &str) -> (TestCore, TestComponent, String) {
    let switchboard = MemorySwitchboard::new();
    let (a, c) = account_pair(code, &switchboard).await;
    let mut ca = manager(&a).await;
    let mut cc = watching(&c).await;
    let struck_id = c.node_id();

    ca.request("devices.revoke", json!({ "device_id": struck_id }))
        .await
        .expect("devices.revoke with no server");
    // A holds the tombstone now — and no longer dials C, which is the point: the
    // only thing left that can carry it is C's OWN dial. Blinking A on the fake
    // LAN is what makes C run a round and dial it.
    switchboard.leave_lan(&a.node_id());
    switchboard.join_lan(&a.node_id());

    // What C's components hear, in order: the directory empties, the session
    // state turns over, and then the one event that says WHY.
    let changed = cc.wait_notification("session.changed").await;
    assert_eq!(changed["logged_in"], json!(false));
    let left = cc.expect_notification("account.left").await;
    assert_eq!(left["reason"], json!("struck_off"));
    (c, cc, struck_id)
}

/// The whole point: the account's decision reaches the device it names, and the
/// device stops being a member — state, disk and keyring, all of it, and nothing
/// of the human's.
#[tokio::test(flavor = "multi_thread")]
async fn the_account_striking_this_device_reaches_it_and_it_leaves() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let (a, c) = account_pair(&code, &switchboard).await;
    let mut ca = manager(&a).await;
    let mut cc = watching(&c).await;
    let struck_id = c.node_id();
    // A file that is NOT the account's: leaving must not touch it.
    let keepsake = c.config_dir().join("keepsake.txt");
    std::fs::write(&keepsake, "the human's, not the account's").expect("write keepsake");
    // And one that IS: tombstones C holds against a third device. C never stores
    // its OWN tombstone (absorb short-circuits), so without this the file would
    // simply never exist on the leaving device — and its erasure would be
    // asserted against nothing.
    seed_revocation(c.config_dir(), &code, &DeviceKey::generate().node_id());

    ca.request("devices.revoke", json!({ "device_id": struck_id }))
        .await
        .expect("devices.revoke with no server");
    switchboard.leave_lan(&a.node_id());
    switchboard.join_lan(&a.node_id());

    // Every device it served leaves the directory first — its own included: a
    // device out of the account knows nobody, not even itself.
    let removed = cc.wait_notification("device.removed").await;
    assert!(removed["device_id"].is_string());
    let changed = cc.wait_notification("session.changed").await;
    assert_eq!(changed["logged_in"], json!(false));
    // Immediately after — same lock, same breath: the sentence the interface
    // owes the human. A logout looks exactly like this without it.
    let left = cc.expect_notification("account.left").await;
    assert_eq!(left["reason"], json!("struck_off"));

    // No longer a member, and no longer ABLE to be one as itself.
    let status = account_status(&mut cc).await;
    assert_eq!(status["attested"], json!(false));
    assert_eq!(status["holds_key"], json!(false));
    let nobody = cc
        .request("devices.list", json!({}))
        .await
        .expect_err("a device out of the account knows of no device at all");
    assert_eq!(nobody.app_code(), "SERVER_UNREACHABLE");

    // The disk: everything that made it a member is gone — `device.key` too,
    // because the tombstone is permanent and only a fresh identity can ever be
    // attested again. The keepsake is untouched: the account owned none of the
    // human's files.
    for gone in [
        "account-key.json",
        "directory.json",
        "revoked.json",
        "device.key",
    ] {
        assert!(
            !c.config_dir().join(gone).exists(),
            "{gone} survived the leave"
        );
    }
    assert!(keepsake.exists(), "the human's file left with the account");
    assert_eq!(
        c.secret("account-key-seed"),
        None,
        "the account's private key survived in the keyring"
    );
}

/// What leaving erases stays erased, and the next startup is a FIRST startup:
/// a fresh identity the old tombstone does not name — the only honest way back.
#[tokio::test(flavor = "multi_thread")]
async fn the_next_startup_is_a_first_startup_under_a_fresh_identity() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let (c, _cc, struck_id) = struck_and_left(&code).await;

    let c = c.restart().await;
    let mut cc = manager(&c).await;
    assert_ne!(
        c.node_id(),
        struck_id,
        "the struck node_id is barred for good: coming back as it would be coming back struck"
    );
    let status = account_status(&mut cc).await;
    assert_eq!(status["attested"], json!(false));
    let nobody = cc
        .request("devices.list", json!({}))
        .await
        .expect_err("still in no account");
    assert_eq!(nobody.app_code(), "SERVER_UNREACHABLE");
}

/// Nothing a peer merely ASSERTS ends an account: a signature that is not the
/// account key's — garbage, or a real key that is somebody else's — wipes
/// nothing, however squarely it names this device. The wipe rides on the same
/// verification as every tombstone.
#[tokio::test(flavor = "multi_thread")]
async fn a_tombstone_a_peer_merely_asserts_wipes_nothing() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let other_code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    // A compromised MEMBER: its record is held, so its streams are served.
    let insider = Stranger::join(&switchboard);
    let core = TestCore::start_in_account_on(
        &code,
        &switchboard,
        DeviceKey::generate(),
        &[insider.record(&code)],
    )
    .await;
    let mut c = watching(&core).await;
    c.drain().await;

    // Garbage where a signature should be, and a REAL signature under another
    // account's key: both name this very device, neither verifies under ours.
    let other_ak =
        onedevice_core::account_key::account_key_from_code(&other_code).expect("valid code");
    for forged in [
        json!("de".repeat(64)),
        json!(onedevice_core::account_key::revoke(
            &other_ak,
            &core.node_id()
        )),
    ] {
        let answer = insider
            .say(
                &core,
                json!({
                    "type": "dir_sync",
                    "records": [],
                    "revoked": { core.node_id(): forged },
                }),
            )
            .await
            .expect("a member's exchange is answered");
        assert_eq!(answer["type"], json!("dir_roster"));
    }

    // Still a member, on every count that leaving would have changed.
    let status = account_status(&mut c).await;
    assert_eq!(status["attested"], json!(true));
    assert_eq!(status["holds_key"], json!(true));
    assert!(core.config_dir().join("device.key").exists());
    assert!(core.config_dir().join("account-key.json").exists());
    c.assert_silent().await;
}

/// The delivery itself, seen on the wire: a struck-off device's dial is answered
/// its own tombstone — one entry, no records, nothing read from its stream — and
/// a mere stranger still gets what it always got: silence. The answer exists
/// only for a `node_id` the account's signature names.
#[tokio::test(flavor = "multi_thread")]
async fn a_struck_off_dial_is_answered_its_tombstone_and_a_strangers_is_not() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let struck = Stranger::join(&switchboard);
    let stranger = Stranger::join(&switchboard);
    // The account struck it off before this Core ever held its record: the
    // tombstone pre-dates the Core, the record is nowhere.
    let dir = tempfile::tempdir().expect("tempdir");
    seed_revocation(dir.path(), &code, &struck.key.node_id());
    let core = TestCore::start_in_account_in(dir, &code, &switchboard).await;

    let answer = struck
        .say(&core, json!({ "type": "dir_sync", "records": [] }))
        .await
        .expect("a struck-off device is answered, not ignored");
    assert_eq!(answer["type"], json!("dir_roster"));
    assert_eq!(answer["records"], json!([]));
    let revoked = answer["revoked"].as_object().expect("a tombstone map");
    assert_eq!(revoked.len(), 1, "its own tombstone and nothing else");
    let ak = onedevice_core::account_key::account_key_from_code(&code).expect("valid test code");
    assert!(
        onedevice_core::account_key::verify_revocation(
            &onedevice_core::account_key::public_hex(&ak),
            &struck.key.node_id(),
            revoked[&struck.key.node_id()]
                .as_str()
                .expect("a signature"),
        ),
        "the answer is the account's own signature: {answer}"
    );

    // The same frame from a device that is merely unknown: silence, as ever.
    assert_eq!(
        stranger
            .say(&core, json!({ "type": "dir_sync", "records": [] }))
            .await,
        None,
        "a stranger must not learn that answers exist"
    );
}

/// A pairing window admits strangers — it must not admit the struck. The answer
/// to a struck-off dial comes BEFORE the window is consulted, whatever frame the
/// dial carries, and the window itself is left untouched for the device the
/// human is actually waiting for.
#[tokio::test(flavor = "multi_thread")]
async fn the_pairing_window_yields_to_the_tombstone() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let struck = Stranger::join(&switchboard);
    let dir = tempfile::tempdir().expect("tempdir");
    seed_revocation(dir.path(), &code, &struck.key.node_id());
    let core = TestCore::start_in_account_in(dir, &code, &switchboard).await;
    let mut c = manager(&core).await;

    let offer = c
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer with no server");

    // Even a pairing offer: the frame is not read at all.
    let answer = struck
        .say(&core, json!({ "type": "lan_pair", "epk": "AA" }))
        .await
        .expect("answered its tombstone, not served the window");
    assert_eq!(answer["type"], json!("dir_roster"));
    assert!(answer["revoked"].as_object().is_some_and(|r| r.len() == 1));

    // And the window never saw it: still open, still waiting for its dialer.
    let untouched = c
        .request(
            "pairing.confirm",
            json!({ "pairing_id": offer["pairing_id"] }),
        )
        .await
        .expect_err("nobody the window would take has dialled");
    assert_eq!(untouched.app_code(), "PAIRING_STATE");
}

/// The mirror guard, on the device that CAN check: a code shown by a struck-off
/// device is refused before anything is dialled — the account's own decision,
/// said as such, not a pairing failure discovered after two humans compared a
/// number for nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_code_shown_by_a_struck_off_device_is_refused() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let struck = Stranger::join(&switchboard);
    let dir = tempfile::tempdir().expect("tempdir");
    seed_revocation(dir.path(), &code, &struck.key.node_id());
    let core = TestCore::start_in_account_in(dir, &code, &switchboard).await;
    let mut c = manager(&core).await;

    let refused = c
        .request("pairing.accept", json!({ "code": struck.code() }))
        .await
        .expect_err("the account struck that device off");
    assert_eq!(refused.app_code(), "DEVICE_REVOKED");
}

/// A pairing in flight dies with the account, and says why: whichever end this
/// device was, it has no account to sponsor into and no standing to join with.
#[tokio::test(flavor = "multi_thread")]
async fn a_pairing_in_flight_dies_with_the_account() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let insider = Stranger::join(&switchboard);
    let core = TestCore::start_in_account_on(
        &code,
        &switchboard,
        DeviceKey::generate(),
        &[insider.record(&code)],
    )
    .await;
    let mut c = watching(&core).await;
    c.request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer with no server");

    // The account's word arrives while the window is open — carried by a
    // sibling's roster this time, which is the other door absorb guards.
    let ak = onedevice_core::account_key::account_key_from_code(&code).expect("valid code");
    insider
        .say(
            &core,
            json!({
                "type": "dir_sync",
                "records": [],
                "revoked": {
                    core.node_id(): onedevice_core::account_key::revoke(&ak, &core.node_id()),
                },
            }),
        )
        .await
        .expect("a member's exchange is answered");

    let left = c.wait_notification("account.left").await;
    assert_eq!(left["reason"], json!("struck_off"));
    let failed = c.expect_notification("pairing.failed").await;
    assert_eq!(
        failed["reason"],
        json!("no_account"),
        "the human watching the code learns the pairing cannot end well: {failed}"
    );
}

/// The answer rides on the signature CHECK, not on the file: a `revoked.json`
/// entry that carries garbage where the account's signature should be speaks for
/// nobody, and the dial it names gets what any stranger gets — silence.
#[tokio::test(flavor = "multi_thread")]
async fn a_tombstone_the_file_merely_contains_does_not_speak() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let stranger = Stranger::join(&switchboard);
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("revoked.json"),
        json!({ "revoked": { stranger.key.node_id(): "de".repeat(64) } }).to_string(),
    )
    .expect("seed a garbled revoked.json");
    let core = TestCore::start_in_account_in(dir, &code, &switchboard).await;

    assert_eq!(
        stranger
            .say(&core, json!({ "type": "dir_sync", "records": [] }))
            .await,
        None,
        "an unverifiable entry must not mint an answer"
    );
}

/// The account's read grants do not outlive it: an open consumer channel is cut
/// and further reads fail `TX_STALE` — the same rule as a logout, applied when
/// it is the account that ends rather than the session.
#[tokio::test(flavor = "multi_thread")]
async fn leaving_cuts_the_clipboards_open_grants() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let insider = Stranger::join(&switchboard);
    let core = TestCore::start_in_account_on(
        &code,
        &switchboard,
        DeviceKey::generate(),
        &[insider.record(&code)],
    )
    .await;
    let mut watcher = watching(&core).await;
    let mut clip = spawn_component(
        &core,
        "clipboard",
        "clipboard-backend",
        &["clipboard.read", "clipboard.write"],
    )
    .await;
    let path = core.write_source("secret.txt", b"secret");
    let tx = clip
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "files" }], "paths": [path.to_string_lossy()] }),
        )
        .await
        .expect("clipboard.updated")["tx_id"]
        .as_str()
        .expect("tx_id")
        .to_string();
    let token = clip
        .request("transactions.open", json!({ "tx_id": tx }))
        .await
        .expect("transactions.open")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string();
    let mut ch = core.open_channel(&token).await;
    assert_eq!(ch.read("f0", 0, 6).await.unwrap(), b"secret");

    let ak = onedevice_core::account_key::account_key_from_code(&code).expect("valid code");
    insider
        .say(
            &core,
            json!({
                "type": "dir_sync",
                "records": [],
                "revoked": {
                    core.node_id(): onedevice_core::account_key::revoke(&ak, &core.node_id()),
                },
            }),
        )
        .await
        .expect("a member's exchange is answered");
    watcher.wait_notification("account.left").await;

    assert_eq!(ch.read("f0", 0, 6).await.unwrap_err(), "TX_STALE");
}
