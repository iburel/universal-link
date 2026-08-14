// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Pairing, IPC side: a device joins the account by being confirmed on a device
//! that is already in it, instead of having the recovery code typed into it
//! (doc/core-api.md, `pairing.*`).
//!
//! Every test here runs against the **real server** (`universallink-server`), so
//! what is exercised is the whole chain: the code displayed on one side, the
//! sealed bundle relayed by a server that cannot read it, the confirmation gated
//! by a fresh ID token, and the enrollment on the grant. The two directions the
//! interface offers are one state machine — whoever displays creates the session,
//! whoever scans claims it — and both are driven below.
//!
//! What these tests pin, beyond the happy paths:
//! - the role is **derived from this device's state**, never chosen by the caller:
//!   sponsoring takes the account key at rest AND a place in the account;
//! - a seed that derives another account's key is **refused**, whatever the human
//!   confirmed on the other side (fail-closed, `account_key::install`);
//! - a device told to sponsor with nothing to sponsor with gives the session back
//!   rather than leaving the other side waiting;
//! - both sides count the deadline themselves (the server says nothing when a
//!   session times out).

use std::time::Duration;

use serde_json::{Value, json};
use universallink_core::{FileSecretStore, SecretStore};

use crate::support::*;

/// Component allowed to drive a pairing: `session.manage` (the methods and the
/// `pairing` topic) + `session.read` (`account.status`) + `devices.read` (to look
/// at the directory the pairing changed).
async fn manager(core: &TestCore) -> TestComponent {
    let mut c = spawn_component(
        core,
        "gui",
        "gui",
        &["session.manage", "session.read", "devices.read"],
    )
    .await;
    c.request(
        "events.subscribe",
        json!({ "topics": ["pairing", "session", "devices"] }),
    )
    .await
    .expect("events.subscribe");
    c
}

/// A Core in the account, holding the account key at rest: what it takes to vouch
/// for anyone.
struct Giver {
    core: TestCore,
    c: TestComponent,
    /// The account's safety number — every device that joins must land on it.
    fingerprint: String,
    /// The recovery code the account was created with: what a device seeded the
    /// OLD way (root on disk, nothing in the keyring) has to be seeded from.
    code: String,
}

async fn sponsor(server: &TestServer) -> Giver {
    let core = TestCore::start_with_server(server).await;
    let mut c = manager(&core).await;
    complete_login(&mut c).await;
    let setup = c
        .request("account.setup", json!({}))
        .await
        .expect("account.setup");
    Giver {
        fingerprint: setup["fingerprint"]
            .as_str()
            .expect("fingerprint")
            .to_string(),
        code: setup["recovery_code"]
            .as_str()
            .expect("recovery_code")
            .to_string(),
        core,
        c,
    }
}

/// `account.status` as `c` sees it.
async fn status(c: &mut TestComponent) -> Value {
    c.request("account.status", json!({}))
        .await
        .expect("account.status")
}

/// The nominal flow of a brand-new PC: it displays a code, a device already in
/// the account scans it, a human confirms — and the newcomer ends up enrolled,
/// attested, and holding the account key, without the recovery code ever being
/// typed into it.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_device_joins_by_being_confirmed_on_another() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let (mut gc, fingerprint) = (giver.c, giver.fingerprint);

    // The newcomer is configured (it knows where the server is) and nothing
    // more: no session, no account.
    let taker = TestCore::start_with_server(&server).await;
    let mut tc = manager(&taker).await;
    let offer = tc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    assert_eq!(
        offer["role"], "joiner",
        "a device with no account key can only be the one joining"
    );
    let code = offer["code"].as_str().expect("code").to_string();
    let pairing_id = offer["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string();
    assert!(
        code.starts_with("UL1:") && code.ends_with(&pairing_id),
        "the code carries its version and its session: {code}"
    );
    assert_eq!(offer["expires_in"], 120);

    // The device in the account scans it, and is told what it is about to vouch
    // for — this is what the human sees.
    let claimed = gc
        .request("pairing.accept", json!({ "code": code }))
        .await
        .expect("pairing.accept");
    assert_eq!(claimed["role"], "sponsor");
    assert_eq!(claimed["pairing_id"], json!(pairing_id));
    assert_eq!(claimed["device"]["name"], CORE_DEVICE_NAME);
    assert_eq!(claimed["device"]["platform"], std::env::consts::OS);
    assert!(
        claimed["device"]["node_id"]
            .as_str()
            .is_some_and(|n| n.len() == 64),
        "the key the human confirms: {}",
        claimed["device"]
    );

    // The newcomer learns someone is there (its side of the same event).
    let told = tc.wait_notification("pairing.claimed").await;
    assert_eq!(told["pairing_id"], json!(pairing_id));
    assert_eq!(
        told.get("device"),
        None,
        "the joiner has no use for its own declaration coming back"
    );
    // The number the human is asked to compare, on BOTH screens: it comes out of
    // the channel key, which a server relaying ciphertext cannot compute — so
    // agreeing on it end to end is what makes the confirmation screen a check
    // rather than a formality.
    assert_eq!(
        told["verification"], claimed["verification"],
        "the two ends of one exchange must show the same number"
    );
    assert!(
        told["verification"]
            .as_str()
            .is_some_and(|v| v.len() == 7 && v.as_bytes()[3] == b' '),
        "six digits to read aloud: {}",
        told["verification"]
    );

    // The side that RECEIVES never confirms — not even now that it holds a
    // channel and could seal something on it. Confirming is the giving side's
    // gesture, and nothing about being claimed changes which side we are on.
    let err = tc
        .request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect_err("a joiner does not confirm, claimed or not");
    assert_eq!(err.app_code(), "PAIRING_STATE");

    // The human confirms. The refresh token in the keyring mints the fresh token
    // the server demands — no browser.
    let confirmed = gc
        .request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect("pairing.confirm");
    assert_eq!(confirmed["status"], "done");
    let done = gc.wait_notification("pairing.completed").await;
    assert_eq!(done["pairing_id"], json!(pairing_id));

    // The newcomer: in the directory, attested under the SAME key, and holding
    // it — so it can vouch for the next device in its turn.
    let done = tc.wait_notification("pairing.completed").await;
    assert_eq!(done["pairing_id"], json!(pairing_id));
    let account = status(&mut tc).await;
    assert_eq!(account["attested"], json!(true));
    assert_eq!(
        account["fingerprint"],
        json!(fingerprint),
        "same account ⇒ same safety number, and nothing was typed to get there"
    );
    assert_eq!(account["holds_key"], json!(true));
    assert!(
        taker.secret("account-key-seed").is_some(),
        "the key must be at rest, not only in memory"
    );

    wait_server_connected(&mut tc, true).await;
    let session = tc
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(session["logged_in"], json!(true));
    assert_eq!(
        session["account"]["email"], TEST_EMAIL,
        "whose account it is, as the sponsor named it"
    );
    // And it is a device like any other, seen by the whole account.
    let own = own_device_id(&mut tc).await;
    wait_directory(
        &mut gc,
        &own,
        |d| d["name"] == CORE_DEVICE_NAME,
        "the newcomer in the directory",
    )
    .await;
    wait_attested(&mut gc, &own).await;
}

/// The other direction, same machinery: the device in the account displays, and
/// the newcomer scans it. This is the phone's gesture.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_device_joins_by_scanning_a_device_of_the_account() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let (mut gc, fingerprint) = (giver.c, giver.fingerprint);

    let offer = gc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    assert_eq!(
        offer["role"], "sponsor",
        "a device in the account, holding the key, is the one that can give"
    );
    let code = offer["code"].as_str().expect("code").to_string();
    let pairing_id = offer["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string();

    let taker = TestCore::start_with_server(&server).await;
    let mut tc = manager(&taker).await;
    let claimed = tc
        .request("pairing.accept", json!({ "code": code }))
        .await
        .expect("pairing.accept");
    assert_eq!(claimed["role"], "joiner");
    assert_eq!(
        claimed.get("device"),
        None,
        "nothing to display: it is the one being displayed"
    );

    // The displaying side is the sponsor here, so it is the one that must show
    // the human what scanned — and both still land on the same number, whichever
    // way round the gesture went.
    let told = gc.wait_notification("pairing.claimed").await;
    assert_eq!(told["device"]["name"], CORE_DEVICE_NAME);
    assert_eq!(told["device"]["platform"], std::env::consts::OS);
    assert_eq!(told["verification"], claimed["verification"]);

    gc.request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect("pairing.confirm");
    tc.wait_notification("pairing.completed").await;

    let account = status(&mut tc).await;
    assert_eq!(account["fingerprint"], json!(fingerprint));
    assert_eq!(account["holds_key"], json!(true));
    wait_server_connected(&mut tc, true).await;
}

/// The way back in, the other one. A device that has the account but not its key
/// is enrolled and attested, and cannot vouch for anyone. Pairing gives it the
/// key with nothing typed — and without a second entry in the directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_cannot_vouch_gets_the_key_by_pairing() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let (mut gc, fingerprint, code) = (giver.c, giver.fingerprint, giver.code);

    // Same account key, seeded on disk as a lost keyring leaves it: the root, and
    // nothing to vouch with.
    let switchboard = universallink_test_support::memory_transport::MemorySwitchboard::new();
    let old = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let mut oc = manager(&old).await;
    wait_server_connected(&mut oc, true).await;
    let before = status(&mut oc).await;
    assert_eq!(before["attested"], json!(true));
    assert_eq!(
        before["holds_key"],
        json!(false),
        "the state this test exists for"
    );
    let devices_before = gc
        .request("devices.list", json!({}))
        .await
        .expect("devices.list")
        .as_array()
        .expect("array")
        .len();

    // It offers as a JOINER — it has nothing to give — over its own
    // authenticated connection, which is what tells the server its account.
    let offer = oc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    assert_eq!(offer["role"], "joiner");
    let pairing_id = offer["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string();
    gc.request(
        "pairing.accept",
        json!({ "code": offer["code"].as_str().expect("code") }),
    )
    .await
    .expect("pairing.accept");
    gc.request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect("pairing.confirm");

    oc.wait_notification("pairing.completed").await;
    let after = status(&mut oc).await;
    assert_eq!(after["holds_key"], json!(true), "it can vouch now");
    assert_eq!(
        after["fingerprint"],
        json!(fingerprint),
        "the same key it was already attested under — nothing moved"
    );
    let devices_after = gc
        .request("devices.list", json!({}))
        .await
        .expect("devices.list")
        .as_array()
        .expect("array")
        .len();
    assert_eq!(
        devices_after, devices_before,
        "a device already in the directory must not enroll a second time"
    );
}

/// The account key received must be the one this device is already attested
/// under. Two devices in the same server account but under different account
/// keys: whatever is confirmed on one, the other refuses a seed that would move
/// it to another trust root (fail-closed).
#[tokio::test(flavor = "multi_thread")]
async fn a_seed_for_another_account_key_is_refused() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let mut gc = giver.c;

    // Attested under a key of its own — a recovery code the sponsor never had.
    let elsewhere = universallink_core::account_key::generate_recovery_code();
    let switchboard = universallink_test_support::memory_transport::MemorySwitchboard::new();
    let mine = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&elsewhere)).await;
    let mut mc = manager(&mine).await;
    wait_server_connected(&mut mc, true).await;
    let before = status(&mut mc).await;

    let offer = mc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let pairing_id = offer["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string();
    gc.request(
        "pairing.accept",
        json!({ "code": offer["code"].as_str().expect("code") }),
    )
    .await
    .expect("pairing.accept");
    gc.request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect("pairing.confirm");

    let failed = mc.wait_notification("pairing.failed").await;
    assert_eq!(
        failed["reason"], "other_account",
        "the bundle opened, and what was inside was refused"
    );
    let after = status(&mut mc).await;
    assert_eq!(
        after["fingerprint"], before["fingerprint"],
        "the trust root must not have moved"
    );
    assert_eq!(
        after["holds_key"],
        json!(false),
        "and nothing of the other account's was kept"
    );
}

/// Told to sponsor with no key to sponsor with: the session goes back rather
/// than leaving the other side to wait out the TTL.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_cannot_vouch_gives_the_session_back() {
    let server = TestServer::start().await;
    let code_of_the_account = universallink_core::account_key::generate_recovery_code();
    let switchboard = universallink_test_support::memory_transport::MemorySwitchboard::new();
    // Enrolled and attested, but holding no key: it is in the account without
    // being able to give it.
    let keyless =
        TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code_of_the_account))
            .await;
    let mut kc = manager(&keyless).await;
    wait_server_connected(&mut kc, true).await;

    let newcomer = TestCore::start_with_server(&server).await;
    let mut nc = manager(&newcomer).await;
    let offer = nc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let err = kc
        .request(
            "pairing.accept",
            json!({ "code": offer["code"].as_str().expect("code") }),
        )
        .await
        .expect_err("a device with no account key cannot sponsor");
    assert_eq!(err.app_code(), "NO_ACCOUNT_KEY");

    // The newcomer is released instead of waiting: the session was given back to
    // the server, which tells the other party.
    let failed = nc.wait_notification("pairing.failed").await;
    assert_eq!(failed["reason"], "declined");
    let account = status(&mut nc).await;
    assert_eq!(account["attested"], json!(false));
}

/// A device enrolled but never attached to the account at all (`attested:
/// false`): pairing is the way in, and the account must SEE it arrive — an
/// attestation nobody was told about authorizes nothing on the data plane.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_never_joined_the_account_gets_in_by_pairing() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let (mut gc, fingerprint) = (giver.c, giver.fingerprint);

    // Enrolled and logged in, with no trust root whatsoever.
    let switchboard = universallink_test_support::memory_transport::MemorySwitchboard::new();
    let outsider = TestCore::start_enrolled_on_with_code(&server, &switchboard, None).await;
    let mut oc = manager(&outsider).await;
    wait_server_connected(&mut oc, true).await;
    assert_eq!(status(&mut oc).await["attested"], json!(false));
    let device_id = own_device_id(&mut oc).await;
    let seen = gc
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(
        find_device(&seen, &device_id)["attestation"],
        Value::Null,
        "nothing to verify it by, which is the state this test starts from"
    );

    let offer = oc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    gc.request(
        "pairing.accept",
        json!({ "code": offer["code"].as_str().expect("code") }),
    )
    .await
    .expect("pairing.accept");
    gc.request(
        "pairing.confirm",
        json!({ "pairing_id": offer["pairing_id"].as_str().expect("pairing_id") }),
    )
    .await
    .expect("pairing.confirm");
    oc.wait_notification("pairing.completed").await;

    assert_eq!(status(&mut oc).await["fingerprint"], json!(fingerprint));
    // Published, not merely kept: the rest of the account can verify it now,
    // without waiting for it to reconnect.
    wait_attested(&mut gc, &device_id).await;
}

/// A sponsor that can no longer read the account key seals nothing — a bundle
/// under some other key would enroll the newcomer into an account of one.
#[tokio::test(flavor = "multi_thread")]
async fn a_sponsor_that_lost_the_key_seals_nothing() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let mut gc = giver.c;

    let taker = TestCore::start_with_server(&server).await;
    let mut tc = manager(&taker).await;
    let offer = tc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let pairing_id = offer["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string();
    gc.request(
        "pairing.accept",
        json!({ "code": offer["code"].as_str().expect("code") }),
    )
    .await
    .expect("pairing.accept");

    // Between the scan and the confirmation, the keyring stops holding it.
    FileSecretStore::new(giver.core.config_dir()).delete("account-key-seed");
    let err = gc
        .request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect_err("nothing to vouch with");
    assert_eq!(err.app_code(), "NO_ACCOUNT_KEY");
    assert_eq!(
        status(&mut tc).await["attested"],
        json!(false),
        "and the newcomer joined nothing at all"
    );
}

/// Confirming before anyone has scanned is refused HERE, without the server
/// being asked: there is no one to seal the account key for, so nothing is
/// sealed and nothing leaves this machine.
#[tokio::test(flavor = "multi_thread")]
async fn confirming_too_early_never_reaches_the_server() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let mut gc = giver.c;

    let offer = gc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    // With the rendezvous gone, anything that had to ask the server would say so.
    server.cut();
    let err = gc
        .request(
            "pairing.confirm",
            json!({ "pairing_id": offer["pairing_id"].as_str().expect("pairing_id") }),
        )
        .await
        .expect_err("nobody has scanned");
    assert_eq!(
        err.app_code(),
        "PAIRING_STATE",
        "a local refusal, not a round trip: SERVER_UNREACHABLE would mean the \
         bundle had already been sealed and sent"
    );
}

/// A cancellation names the pairing it means. A dialog that closes late must not
/// take down the pairing that replaced the one it was showing.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_cancellation_settles_nothing() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let mut gc = giver.c;

    let taker = TestCore::start_with_server(&server).await;
    let mut tc = manager(&taker).await;
    let offer = tc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let pairing_id = offer["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string();
    gc.request(
        "pairing.accept",
        json!({ "code": offer["code"].as_str().expect("code") }),
    )
    .await
    .expect("pairing.accept");

    gc.request(
        "pairing.cancel",
        json!({ "pairing_id": "p_somewhere_else" }),
    )
    .await
    .expect("cancelling something else is not an error");
    // The live one is untouched, and still confirmable.
    gc.request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect("the pairing must have survived a cancellation meant for another");
    tc.wait_notification("pairing.completed").await;
}

/// A confirmation whose refresh token is gone goes through the browser, exactly
/// like a revocation's — and the pairing still lands.
#[tokio::test(flavor = "multi_thread")]
async fn a_confirmation_can_take_the_browser_route() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let (mut gc, fingerprint) = (giver.c, giver.fingerprint);

    let taker = TestCore::start_with_server(&server).await;
    let mut tc = manager(&taker).await;
    let offer = tc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let pairing_id = offer["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string();
    gc.request(
        "pairing.accept",
        json!({ "code": offer["code"].as_str().expect("code") }),
    )
    .await
    .expect("pairing.accept");

    // The keyring no longer holds what mints a fresh token browserlessly.
    FileSecretStore::new(giver.core.config_dir()).delete("oidc-refresh-token");
    let asked = gc
        .request("pairing.confirm", json!({ "pairing_id": pairing_id }))
        .await
        .expect("pairing.confirm");
    assert_eq!(asked["status"], "reauth_required");
    let page = browse(asked["auth_url"].as_str().expect("auth_url"))
        .await
        .expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);

    // The outcome reaches the caller through the events: its request answered
    // long before the browser did.
    let done = gc.wait_notification("pairing.completed").await;
    assert_eq!(done["pairing_id"], json!(pairing_id));
    tc.wait_notification("pairing.completed").await;
    assert_eq!(status(&mut tc).await["fingerprint"], json!(fingerprint));
    assert!(
        giver.core.secret("oidc-refresh-token").is_some(),
        "the fresh token that came back is stowed, as a revocation's re-auth does"
    );
}

/// The human declined: the other side is told at once instead of waiting out the
/// TTL, and nothing was installed.
#[tokio::test(flavor = "multi_thread")]
async fn a_declined_confirmation_releases_the_other_side() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let mut gc = giver.c;

    let taker = TestCore::start_with_server(&server).await;
    let mut tc = manager(&taker).await;
    let offer = tc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let pairing_id = offer["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string();
    gc.request(
        "pairing.accept",
        json!({ "code": offer["code"].as_str().expect("code") }),
    )
    .await
    .expect("pairing.accept");

    gc.request("pairing.cancel", json!({ "pairing_id": pairing_id }))
        .await
        .expect("pairing.cancel");
    let failed = tc.wait_notification("pairing.failed").await;
    assert_eq!(failed["pairing_id"], json!(pairing_id));
    assert_eq!(failed["reason"], "declined");
    assert_eq!(status(&mut tc).await["attested"], json!(false));

    // Cancelling twice, or cancelling something we no longer have, is not an
    // error: the dialog it came from was closing either way.
    gc.request("pairing.cancel", json!({ "pairing_id": pairing_id }))
        .await
        .expect("cancel is idempotent");
    gc.request("pairing.cancel", json!({ "pairing_id": "p_nothing" }))
        .await
        .expect("cancel is idempotent");
}

/// The server says nothing when a session times out — both sides were told the
/// deadline. This is the Core keeping that count.
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_code_expires_on_its_own() {
    let server = TestServer::start_with(|c| c.pairing_ttl = Duration::from_secs(1)).await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = manager(&core).await;

    let offer = c
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    assert_eq!(offer["expires_in"], 1);
    let failed = c.wait_notification("pairing.failed").await;
    assert_eq!(failed["pairing_id"], offer["pairing_id"]);
    assert_eq!(failed["reason"], "expired");
}

/// The side that SCANNED counts the deadline too, and counts the one the server
/// gave it: it did not create the session, so `pairing.claim`'s answer is the only
/// place it can learn how much of the TTL is left.
#[tokio::test(flavor = "multi_thread")]
async fn a_scanned_code_expires_on_the_scanner_too() {
    let server = TestServer::start_with(|c| c.pairing_ttl = Duration::from_secs(1)).await;
    let giver = sponsor(&server).await;
    let mut gc = giver.c;

    let offer = gc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let taker = TestCore::start_with_server(&server).await;
    let mut tc = manager(&taker).await;
    tc.request(
        "pairing.accept",
        json!({ "code": offer["code"].as_str().expect("code") }),
    )
    .await
    .expect("pairing.accept");

    // Nobody confirms. The scanner gives up on its own — the server never says a
    // word about a session that timed out.
    let failed = tc.wait_notification("pairing.failed").await;
    assert_eq!(failed["reason"], "expired");
}

/// What the API refuses, and how it says so.
#[tokio::test(flavor = "multi_thread")]
async fn what_pairing_refuses() {
    let server = TestServer::start().await;
    let giver = sponsor(&server).await;
    let mut gc = giver.c;

    // A code that is not one never reaches the server.
    for wrong in ["", "nope", "UL2:a:b:c"] {
        let err = gc
            .request("pairing.accept", json!({ "code": wrong }))
            .await
            .expect_err("not a pairing code");
        assert_eq!(err.code, -32602, "{wrong:?} → {err:?}");
    }
    let err = gc
        .request("pairing.accept", json!({}))
        .await
        .expect_err("code is required");
    assert_eq!(err.code, -32602);

    // Confirming needs a pairing, and a claimed one at that.
    let err = gc
        .request("pairing.confirm", json!({ "pairing_id": "p_nothing" }))
        .await
        .expect_err("unknown pairing");
    assert_eq!(err.app_code(), "PAIRING_UNKNOWN");
    let offer = gc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let err = gc
        .request(
            "pairing.confirm",
            json!({ "pairing_id": offer["pairing_id"].as_str().expect("pairing_id") }),
        )
        .await
        .expect_err("nobody has scanned yet");
    assert_eq!(
        err.app_code(),
        "PAIRING_STATE",
        "there is no one to seal the account for"
    );

    // A joiner has nothing to confirm.
    let taker = TestCore::start_with_server(&server).await;
    let mut tc = manager(&taker).await;
    let offered = tc
        .request("pairing.offer", json!({}))
        .await
        .expect("pairing.offer");
    let err = tc
        .request(
            "pairing.confirm",
            json!({ "pairing_id": offered["pairing_id"].as_str().expect("pairing_id") }),
        )
        .await
        .expect_err("a joiner does not confirm");
    assert_eq!(err.app_code(), "PAIRING_STATE");
}

/// A device that HAS a server pairs through it, and cannot pair at all while it
/// cannot reach it: the rendezvous is the server's to be. (With no server at all
/// there is nothing to be unreachable for, and the code is dialled on the local
/// network instead — `lanpair.rs`.)
#[tokio::test(flavor = "multi_thread")]
async fn pairing_through_a_server_needs_that_server() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = manager(&core).await;
    server.cut();
    let err = c
        .request("pairing.offer", json!({}))
        .await
        .expect_err("the rendezvous is unreachable");
    assert_eq!(err.app_code(), "SERVER_UNREACHABLE");
}

/// The methods and the topic are gated on `session.manage`: whoever watches a
/// pairing is whoever may answer it.
#[tokio::test(flavor = "multi_thread")]
async fn pairing_is_gated_on_session_manage() {
    let core = TestCore::start().await;
    let mut c = spawn_component(&core, "watcher", "custom", &["session.read"]).await;
    for method in [
        "pairing.offer",
        "pairing.accept",
        "pairing.confirm",
        "pairing.cancel",
    ] {
        let err = c
            .request(method, json!({ "code": "x", "pairing_id": "x" }))
            .await
            .expect_err("scope required");
        assert_eq!(err.app_code(), "SCOPE_DENIED", "{method}");
    }
    let err = c
        .request("events.subscribe", json!({ "topics": ["pairing"] }))
        .await
        .expect_err("topic scope required");
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}
