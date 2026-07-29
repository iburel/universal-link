// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Account key (C7), IPC side: onboarding via recovery code and out-of-band
//! verification by fingerprint (safety number). The first device CREATES the
//! key (`account.setup`), the rest JOIN it (`account.join`); one same key ⇒ one
//! same fingerprint, to be compared visually between devices.
//! (The proof that this root actually authorizes peers on the data plane is in
//! `dataplane.rs` — the harness seeds there exactly what `account.join` writes,
//! which also means those Cores hold no account key: the data plane never needs
//! one, only `ak_pub`.)
//!
//! The account's private key is also kept AT REST, in the keyring, so a device
//! can later vouch for a joining one. `account.status` answers `holds_key` by
//! reading it back, and refuses a seed that is not this account's — the tests
//! below pin both, plus the way back in for a device that has the account and not
//! its key.

use serde_json::json;
use universallink_test_support::memory_transport::MemorySwitchboard;

use crate::support::*;

/// GUI component able to manage the account: `session.manage` (login, account
/// key) + `session.read` (status).
async fn manager(core: &TestCore) -> TestComponent {
    spawn_component(core, "gui", "gui", &["session.manage", "session.read"]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_then_join_converge_on_one_fingerprint() {
    let server = TestServer::start().await;

    // First of all, no account is attested.
    let a = TestCore::start_with_server(&server).await;
    let mut ac = manager(&a).await;
    let status = ac
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(false));
    assert_eq!(status["fingerprint"], json!(null));

    // A logs in then CREATES the account key: a recovery code is returned to it
    // (the user's way back if every device is lost) along with the fingerprint
    // to compare.
    complete_login(&mut ac).await;
    let setup = ac
        .request("account.setup", json!({}))
        .await
        .expect("account.setup");
    let code = setup["recovery_code"]
        .as_str()
        .expect("recovery_code")
        .to_string();
    let fp = setup["fingerprint"]
        .as_str()
        .expect("fingerprint")
        .to_string();
    assert_eq!(fp.split(' ').count(), 6, "fingerprint in 6 groups: {fp}");

    // The status reflects the attested state and returns the same fingerprint.
    let status = ac
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true));
    assert_eq!(status["fingerprint"], json!(fp));
    // And the account's private key is HELD, not discarded: that is what will
    // let this device vouch for a joining one without the code being retyped.
    assert_eq!(status["holds_key"], json!(true));
    assert!(
        a.secret("account-key-seed").is_some(),
        "the key must be at rest in the keyring, not only in memory"
    );

    // Re-creating the key is refused: replacing it (rotation) is a follow-up
    // building block, not a trivial gesture.
    let again = ac
        .request("account.setup", json!({}))
        .await
        .expect_err("re-setup refused");
    assert_eq!(again.app_code(), "ACCOUNT_KEY_SET");

    // B (another device of the same account) JOINS with A's code → SAME
    // fingerprint: this is the anchor of out-of-band verification.
    let b = TestCore::start_with_server(&server).await;
    let mut bc = manager(&b).await;
    complete_login(&mut bc).await;
    let join = bc
        .request("account.join", json!({ "recovery_code": code }))
        .await
        .expect("account.join");
    assert_eq!(
        join["fingerprint"],
        json!(fp),
        "same code ⇒ same fingerprint (out-of-band verification)"
    );
    let status = bc
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["holds_key"], json!(true), "B holds the key too");

    // Re-entering the code of the account this device is ALREADY in goes
    // through, and changes nothing: same key, so byte-for-byte the same
    // attestation. It is also the way back in for a device that lost the key —
    // see the test below.
    let again = bc
        .request("account.join", json!({ "recovery_code": code }))
        .await
        .expect("re-entering the same code is idempotent");
    assert_eq!(again["fingerprint"], json!(fp));

    // ANOTHER code, however, is refused: replacing the root is a rotation, a
    // follow-up building block — and it is the surface an attacker with
    // `session.manage` would use to re-attest this device under a key of their
    // own choosing.
    let elsewhere = universallink_core::account_key::generate_recovery_code();
    let rotate = bc
        .request("account.join", json!({ "recovery_code": elsewhere }))
        .await
        .expect_err("another account key refused");
    assert_eq!(rotate.app_code(), "ACCOUNT_KEY_SET");
    let status = bc
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(
        status["fingerprint"],
        json!(fp),
        "a refused rotation must leave the root alone"
    );
}

/// The way back in, one of the two. A device that has the account but not its key
/// — a keyring that lost the seed, or one that never answered — verifies peers
/// but has nothing to hand a joining device. Typing the recovery code, and
/// nothing else, gives it back.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_without_the_key_gets_it_back_from_the_code() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    // The harness seeds `account-key.json` alone — the state of a device whose
    // keyring no longer holds the seed.
    let core = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let mut c = manager(&core).await;
    // `account.join` republishes the attestation, so it wants the server: this
    // Core reconnects on its own from `session.json`, we just wait for it.
    wait_server_connected(&mut c, true).await;

    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true), "it IS in the account");
    assert_eq!(
        status["holds_key"],
        json!(false),
        "…but holds nothing it could vouch with"
    );
    let fp = status["fingerprint"]
        .as_str()
        .expect("fingerprint")
        .to_string();

    // The way back: the same code it joined with.
    let joined = c
        .request("account.join", json!({ "recovery_code": code }))
        .await
        .expect("account.join");
    assert_eq!(
        joined["fingerprint"],
        json!(fp),
        "same account: the fingerprint cannot move"
    );
    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["holds_key"], json!(true));
}

/// The keyring does not get to choose the key we sign with. Whoever can write it
/// — but not `account-key.json`, which pins `ak_pub` — must not make this device
/// vouch for a joining one under a foreign account key: that device would join
/// THEIR account believing it joined ours.
#[tokio::test(flavor = "multi_thread")]
async fn a_planted_seed_is_not_taken_for_the_account_key() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = manager(&core).await;
    complete_login(&mut c).await;
    c.request("account.setup", json!({}))
        .await
        .expect("account.setup");

    // Someone with write access to the keyring — and nothing else — swaps the
    // stored seed for the key of an account of their own.
    let theirs = universallink_core::account_key::account_key_from_code(
        &universallink_core::account_key::generate_recovery_code(),
    )
    .expect("valid code");
    let keyring = universallink_core::FileSecretStore::new(core.config_dir());
    universallink_core::account_key::remember(&keyring, &theirs).expect("plant the seed");

    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(
        status["attested"],
        json!(true),
        "the root is untouched: the device is still in the account"
    );
    assert_eq!(
        status["holds_key"],
        json!(false),
        "a seed that is not this account's key counts as no key at all"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mistyped_recovery_code_is_rejected() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = manager(&core).await;
    complete_login(&mut c).await;

    // A code outside the alphabet / whose checksum does not add up: refused
    // BEFORE any persistence (no root is written).
    let bad = c
        .request(
            "account.join",
            json!({ "recovery_code": "NOT-A-REAL-CODE" }),
        )
        .await
        .expect_err("invalid code");
    assert_eq!(bad.app_code(), "INVALID_CODE");

    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(
        status["attested"],
        json!(false),
        "a refused code must attest nothing"
    );
}

/// The order in which onboarding persists things, seen from outside: the key
/// goes in BEFORE the root. A keyring that refuses must therefore leave nothing
/// behind — because the root's own guard would answer `ACCOUNT_KEY_SET` on the
/// retry, and the device would be stuck attested-but-keyless with no way out
/// through the API.
#[tokio::test(flavor = "multi_thread")]
async fn a_keyring_that_refuses_the_key_installs_nothing() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = manager(&core).await;
    complete_login(&mut c).await;

    // A directory where `secrets.json` belongs: every write to the fallback
    // keyring now fails. (Done after the login, which stows the refresh token
    // there.)
    let secrets = core.config_dir().join("secrets.json");
    std::fs::remove_file(&secrets).ok();
    std::fs::create_dir(&secrets).expect("block the keyring");

    let refused = c
        .request("account.setup", json!({}))
        .await
        .expect_err("no key stowed, no account");
    assert_eq!(refused.app_code(), "ACCOUNT_KEY_SAVE_FAILED");
    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(
        status["attested"],
        json!(false),
        "the root must not be installed without the key"
    );
    assert!(
        !core.config_dir().join("account-key.json").exists(),
        "nor written to disk"
    );

    // And the retry, once the keyring answers again, goes through — which is the
    // whole point of that ordering.
    std::fs::remove_dir(&secrets).expect("unblock the keyring");
    c.request("account.setup", json!({}))
        .await
        .expect("account.setup retried");
    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true));
    assert_eq!(status["holds_key"], json!(true));
}

/// A revocation takes the refresh token away and leaves the account key —
/// deliberately. Vouching for a joining device requires being authenticated in
/// the account at the server's rendezvous, which this revocation just took away,
/// so erasing the key buys nothing; and it would cost a recovery code retyped
/// after every revocation the user did not mean. Whoever decides otherwise
/// should have to change this test on purpose.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_device_keeps_the_account_key() {
    let server = TestServer::start().await;
    let mut admin = server.online_device("PC-Admin", "macos").await;
    let core = TestCore::start_enrolled(&server).await;
    let mut c = manager(&core).await;
    wait_server_connected(&mut c, true).await;
    c.request("account.setup", json!({}))
        .await
        .expect("account.setup");

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
    assert!(
        core.secret("oidc-refresh-token").is_none(),
        "the refresh token belonged to the session: it leaves with it"
    );

    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true), "still in the account");
    assert_eq!(
        status["holds_key"],
        json!(true),
        "and still holding its key: a re-enrollment must not need the code again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn account_setup_requires_session_manage() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    // A read-only component cannot touch the account key.
    let mut c = spawn_component(&core, "obs", "tray", &["session.read"]).await;
    let denied = c
        .request("account.setup", json!({}))
        .await
        .expect_err("setup without session.manage");
    assert_eq!(denied.app_code(), "SCOPE_DENIED");
}
