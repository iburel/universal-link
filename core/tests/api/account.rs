// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Account key (C7), IPC side: onboarding via recovery code and out-of-band
//! verification by fingerprint (safety number). The first device CREATES the
//! key (`account.setup`), the rest JOIN it (`account.join`); one same key ⇒ one
//! same fingerprint, to be compared visually between devices.
//! (The proof that this root actually authorizes peers on the data plane is in
//! `dataplane.rs` — the harness seeds there exactly what `account.join`
//! writes.)

use serde_json::json;

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
    // (the only copy of AK_priv) along with the fingerprint to compare.
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

    // Re-joining once the key is set is refused: overwriting an existing root
    // (rotation) is a follow-up building block, not a trivial gesture — it is
    // also the surface an attacker would use to re-attest under a chosen code.
    let rejoin = bc
        .request("account.join", json!({ "recovery_code": code }))
        .await
        .expect_err("re-join refused");
    assert_eq!(rejoin.app_code(), "ACCOUNT_KEY_SET");
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
