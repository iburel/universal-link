// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Pairing: a device joins the account by being confirmed on a device that is
//! already in it (`doc/server-api.md`, "Pairing").
//!
//! The suite drives BOTH directions, because they are one state machine seen
//! from two ends: whoever displays the QR code creates the session, whoever
//! scans it claims the session, and the role says which of them is joining.
//!
//! `channel` and `bundle` are opaque strings here on purpose — that is exactly
//! what they are to the server. The values below are not key material, and the
//! tests assert only that they come out the other side untouched.

use serde_json::{Value, json};

use super::support::*;

/// Stand-ins for the two sides' public channel material. Any string will do:
/// the server relays it without looking.
const JOINER_CHANNEL: &str = "11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee";
const SPONSOR_CHANNEL: &str = "22dd22dd22dd22dd22dd22dd22dd22dd22dd22dd22dd22dd22dd22dd22dd22dd";
/// Stand-in for the sealed bundle. Ciphertext, as far as the server is
/// concerned — it must arrive byte for byte.
const BUNDLE: &str = "c1pher-t3xt::the-account-key-seed-and-nothing-the-server-can-read";

/// What a joining device declares about itself.
fn joining(name: &str, platform: &str, node_id: &str) -> Value {
    json!({ "name": name, "platform": platform, "node_id": node_id })
}

/// A device that has not joined anything: its own connection and its own key.
struct Joiner {
    conn: TestConn,
    key: DeviceKey,
}

impl Joiner {
    async fn arrive(env: &TestEnv) -> Joiner {
        Joiner {
            conn: env.connect().await,
            key: DeviceKey::generate(),
        }
    }

    fn declaration(&self, name: &str) -> Value {
        joining(name, "linux", &self.key.node_id())
    }

    /// `auth.enroll` on an approved pairing: no ID token, just the proof that
    /// this really is the key the session pinned.
    async fn enroll_with(&mut self, pairing_id: &str) -> Result<Value, RpcError> {
        let nonce = challenge(&mut self.conn).await;
        self.conn
            .request(
                "auth.enroll",
                json!({ "pairing_id": pairing_id, "proof": self.key.proof(&nonce) }),
            )
            .await
    }
}

/// The nominal flow of a brand-new PC: it displays, a device already in the
/// account scans it, a human confirms.
#[tokio::test]
async fn a_new_device_joins_by_being_confirmed_on_a_device_of_the_account() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;

    // 1. The newcomer opens a session and shows it. It has no account — that is
    //    the whole point of pairing — so this call is unauthenticated.
    let offered = joiner
        .conn
        .request(
            "pairing.create",
            json!({
                "role": "joiner",
                "channel": JOINER_CHANNEL,
                "device": joiner.declaration("New-PC"),
            }),
        )
        .await
        .expect("pairing.create");
    let pairing_id = offered["pairing_id"].as_str().expect("pairing_id");
    assert!(
        pairing_id.starts_with("p_"),
        "unexpected id shape: {pairing_id}"
    );
    assert_eq!(
        offered["expires_in"], 120,
        "the offerer must be told how long its code lives"
    );

    // 2. The device in the account scans it. It is told what it is about to
    //    vouch for — this is what the human sees before confirming.
    let claimed = sponsor
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.claim");
    assert_eq!(
        claimed["role"], "sponsor",
        "the scanner is on the account's side here"
    );
    assert_eq!(claimed["device"]["name"], "New-PC");
    assert_eq!(claimed["device"]["platform"], "linux");
    assert_eq!(claimed["device"]["node_id"], joiner.key.node_id());
    // The claimer is told the deadline too. It did not create the session, so
    // this answer is the only place it can learn one — and both sides time out
    // on their own clocks (no `expired` notification exists).
    let left = claimed["expires_in"].as_u64().expect("expires_in");
    assert!(
        (1..=120).contains(&left),
        "the time left must be what remains of the TTL, not a fresh one: {left}"
    );

    // 3. The newcomer learns the other side is there, and gets its channel.
    let told = joiner.conn.expect_notification("pairing.claimed").await;
    assert_eq!(told["channel"], SPONSOR_CHANNEL);
    assert_eq!(
        told.get("device"),
        None,
        "the joiner has no use for its own declaration coming back"
    );

    // 4. The human confirms. The sealed bundle crosses, untouched.
    sponsor
        .conn
        .request(
            "pairing.approve",
            json!({
                "pairing_id": pairing_id,
                "id_token": env.oidc.id_token(TEST_SUB),
                "bundle": BUNDLE,
            }),
        )
        .await
        .expect("pairing.approve");
    let completed = joiner.conn.expect_notification("pairing.completed").await;
    assert_eq!(
        completed["bundle"], BUNDLE,
        "the bundle must arrive byte for byte: the server cannot read it, so it \
         had better not touch it"
    );

    // 5. The session is now the enrollment grant. No ID token anywhere.
    let enrolled = joiner
        .enroll_with(pairing_id)
        .await
        .expect("enrollment on the pairing");
    assert_eq!(enrolled["api_version"], 1);
    assert!(
        enrolled["device_id"]
            .as_str()
            .expect("device_id")
            .starts_with("d_")
    );
    assert_eq!(enrolled["device"]["name"], "New-PC");
    assert_eq!(enrolled["device"]["node_id"], joiner.key.node_id());
    assert_eq!(
        enrolled["device"]["attestation"],
        Value::Null,
        "joining the account and attesting to it are two different things (C7)"
    );

    // The account's other devices see it arrive, and it is a device like any
    // other: it authenticates on its own key from now on.
    let added = sponsor.conn.expect_notification("device.added").await;
    assert_eq!(added["device"]["name"], "New-PC");
    let device_id = enrolled["device_id"].as_str().expect("device_id");
    let own = authenticate(&mut joiner.conn, &joiner.key, device_id).await;
    assert_eq!(own["online"], true);

    let list = sponsor
        .conn
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(list.as_array().expect("list").len(), 2);
    assert_eq!(find_device(&list, device_id)["name"], "New-PC");
}

/// The other direction, same machinery: a device already in the account
/// displays, and the newcomer — a phone, typically — scans it.
#[tokio::test]
async fn a_new_device_joins_by_scanning_a_device_of_the_account() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Office-PC", "linux").await;
    let mut joiner = Joiner::arrive(&env).await;

    let offered = sponsor
        .conn
        .request(
            "pairing.create",
            json!({ "role": "sponsor", "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.create");
    let pairing_id = offered["pairing_id"].as_str().expect("pairing_id");

    let claimed = joiner
        .conn
        .request(
            "pairing.claim",
            json!({
                "pairing_id": pairing_id,
                "channel": JOINER_CHANNEL,
                "device": joining("New-Phone", "android", &joiner.key.node_id()),
            }),
        )
        .await
        .expect("pairing.claim");
    assert_eq!(
        claimed["role"], "joiner",
        "the scanner is the newcomer in this direction"
    );
    assert_eq!(
        claimed.get("device"),
        None,
        "nothing to display: it is the one being displayed"
    );
    assert!(
        (1..=120).contains(&claimed["expires_in"].as_u64().expect("expires_in")),
        "the claimer is told the deadline whichever side it is on"
    );

    // The displaying device is the sponsor here, so it is the one that receives
    // the declaration to put in front of the human.
    let told = sponsor.conn.expect_notification("pairing.claimed").await;
    assert_eq!(told["channel"], JOINER_CHANNEL);
    assert_eq!(told["device"]["name"], "New-Phone");
    assert_eq!(told["device"]["platform"], "android");
    assert_eq!(told["device"]["node_id"], joiner.key.node_id());

    sponsor
        .conn
        .request(
            "pairing.approve",
            json!({
                "pairing_id": pairing_id,
                "id_token": env.oidc.id_token(TEST_SUB),
                "bundle": BUNDLE,
            }),
        )
        .await
        .expect("pairing.approve");
    assert_eq!(
        joiner.conn.expect_notification("pairing.completed").await["bundle"],
        BUNDLE
    );

    let enrolled = joiner
        .enroll_with(pairing_id)
        .await
        .expect("enrollment on the pairing");
    assert_eq!(enrolled["device"]["name"], "New-Phone");
    assert_eq!(enrolled["device"]["platform"], "android");
}

/// The point of pinning: a human confirms a named device with a given key, and
/// that is what joins the account. The enrolling request gets no say — neither
/// by restating the record, nor by holding a different key.
#[tokio::test]
async fn what_enrolls_is_what_was_confirmed() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer_and_confirm(&env, &mut sponsor, &mut joiner, "New-PC").await;

    // A proof by ANOTHER key is not the pinned device: refused, and the grant
    // survives for the device that was actually confirmed.
    let impostor = DeviceKey::generate();
    let nonce = challenge(&mut joiner.conn).await;
    let refused = joiner
        .conn
        .request(
            "auth.enroll",
            json!({ "pairing_id": pairing_id, "proof": impostor.proof(&nonce) }),
        )
        .await
        .expect_err("a proof by another key must not enroll");
    assert_eq!(refused.app_code(), "INVALID_PROOF");

    // Restating the record changes nothing: the session is the authority.
    let nonce = challenge(&mut joiner.conn).await;
    let enrolled = joiner
        .conn
        .request(
            "auth.enroll",
            json!({
                "pairing_id": pairing_id,
                "proof": joiner.key.proof(&nonce),
                "name": "Something-Else",
                "platform": "windows",
                "node_id": impostor.node_id(),
            }),
        )
        .await
        .expect("enrollment on the pairing");
    assert_eq!(enrolled["device"]["name"], "New-PC");
    assert_eq!(enrolled["device"]["platform"], "linux");
    assert_eq!(enrolled["device"]["node_id"], joiner.key.node_id());
}

/// One confirmation, one device. A grant that could be spent twice would let a
/// single human "yes" bring in a second machine.
#[tokio::test]
async fn a_confirmation_enrolls_exactly_one_device() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer_and_confirm(&env, &mut sponsor, &mut joiner, "New-PC").await;

    joiner.enroll_with(&pairing_id).await.expect("first");
    let refused = joiner
        .enroll_with(&pairing_id)
        .await
        .expect_err("a spent grant must not enroll again");
    assert_eq!(refused.app_code(), "PAIRING_UNKNOWN");
}

/// Confirming is not something the account's other devices get to do on the
/// scanner's behalf: the one that took part is the one that answers.
#[tokio::test]
async fn only_the_device_that_scanned_confirms() {
    let env = TestEnv::start().await;
    let mut scanner = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut bystander = online_device(&env, TEST_SUB, "Other-PC", "linux").await;
    let mut joiner = Joiner::arrive(&env).await;

    let pairing_id = offer(&mut joiner, "New-PC").await;
    scanner
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.claim");
    joiner.conn.expect_notification("pairing.claimed").await;

    // Same account, valid fresh token, and it even knows the id: still not the
    // device the human is standing in front of.
    let refused = bystander
        .conn
        .request(
            "pairing.approve",
            json!({
                "pairing_id": &pairing_id,
                "id_token": env.oidc.id_token(TEST_SUB),
                "bundle": BUNDLE,
            }),
        )
        .await
        .expect_err("a bystander must not confirm");
    assert_eq!(refused.app_code(), "PAIRING_STATE");
    joiner.conn.assert_silent().await;
}

/// A pairing joins exactly two devices. A second scanner does not get to
/// displace the one the human is about to confirm.
#[tokio::test]
async fn a_second_scanner_is_turned_away() {
    let env = TestEnv::start().await;
    let mut first = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut second = online_device(&env, TEST_SUB, "Other-PC", "linux").await;
    let mut joiner = Joiner::arrive(&env).await;

    let pairing_id = offer(&mut joiner, "New-PC").await;
    first
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.claim");
    let refused = second
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect_err("a claimed pairing must not be claimed again");
    assert_eq!(refused.app_code(), "PAIRING_STATE");
}

/// Nothing enrolls before a human has said yes. This is the one rule the whole
/// feature exists to enforce, so it is checked from the joiner's own connection,
/// mid-flow, with the scan already done.
#[tokio::test]
async fn enrolling_before_the_confirmation_is_refused() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer(&mut joiner, "New-PC").await;

    let refused = joiner
        .enroll_with(&pairing_id)
        .await
        .expect_err("an unscanned session is not a grant");
    assert_eq!(refused.app_code(), "PAIRING_STATE");

    sponsor
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.claim");
    let refused = joiner
        .enroll_with(&pairing_id)
        .await
        .expect_err("scanned is not confirmed");
    assert_eq!(refused.app_code(), "PAIRING_STATE");
}

/// A sponsor cannot confirm into thin air. This is the direction where the
/// sponsor holds the session itself, so the stage is the only thing standing
/// between "nobody scanned" and a confirmation addressed to no one.
#[tokio::test]
async fn a_sponsor_cannot_confirm_before_anyone_scanned() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Office-PC", "linux").await;

    let offered = sponsor
        .conn
        .request(
            "pairing.create",
            json!({ "role": "sponsor", "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.create");
    let pairing_id = offered["pairing_id"].as_str().expect("pairing_id");

    let refused = sponsor
        .conn
        .request(
            "pairing.approve",
            json!({
                "pairing_id": pairing_id,
                "id_token": env.oidc.id_token(TEST_SUB),
                "bundle": BUNDLE,
            }),
        )
        .await
        .expect_err("there is nobody to confirm");
    assert_eq!(refused.app_code(), "PAIRING_STATE");
}

/// One connection can rebind to another device — including a device of another
/// account (`auth.authenticate` allows it). A pairing it claimed beforehand must
/// not follow it there: the account a session is about is settled at the scan,
/// and the connection identity alone no longer says which one that was.
#[tokio::test]
async fn a_connection_that_switched_account_cannot_confirm() {
    let env = TestEnv::start().await;
    let mut alice = online_device(&env, "alice", "Alice-PC", "linux").await;
    // Enrolled under another account, on its own connection, and never
    // authenticated there: free for Alice's connection to bind to.
    let bob = enroll_device(&env, "bob", "Bob-PC", "linux").await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer(&mut joiner, "New-PC").await;

    alice
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.claim");
    joiner.conn.expect_notification("pairing.claimed").await;

    // Same connection, now speaking for Bob.
    authenticate(&mut alice.conn, &bob.key, &bob.device_id).await;
    let refused = alice
        .conn
        .request(
            "pairing.approve",
            json!({
                "pairing_id": &pairing_id,
                "id_token": env.oidc.id_token("bob"),
                "bundle": BUNDLE,
            }),
        )
        .await
        .expect_err("Bob must not confirm what Alice scanned");
    assert_eq!(refused.app_code(), "PAIRING_UNKNOWN");
    joiner.conn.assert_silent().await;
}

/// The grant is the joiner's, not the pairing id's: knowing the id is not being
/// the device that was confirmed.
#[tokio::test]
async fn only_the_joiner_spends_the_grant() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer_and_confirm(&env, &mut sponsor, &mut joiner, "New-PC").await;

    let mut outsider = Joiner::arrive(&env).await;
    let refused = outsider
        .enroll_with(&pairing_id)
        .await
        .expect_err("another connection must not spend the grant");
    assert_eq!(refused.app_code(), "PAIRING_UNKNOWN");

    // And the confirmed device is untouched by the attempt.
    joiner
        .enroll_with(&pairing_id)
        .await
        .expect("the grant still belongs to the joiner");
}

/// The side that gives the account away must be in it. An unauthenticated
/// scanner has nothing to vouch with.
#[tokio::test]
async fn an_unauthenticated_scanner_cannot_sponsor() {
    let env = TestEnv::start().await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer(&mut joiner, "New-PC").await;

    let mut stranger = env.connect().await;
    let refused = stranger
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect_err("only a device of the account sponsors");
    assert_eq!(refused.app_code(), "NOT_AUTHENTICATED");
}

/// Confirming is a sensitive operation, gated like `devices.revoke`: a fresh
/// token, and one belonging to the account being handed over.
#[tokio::test]
async fn confirming_demands_a_fresh_token_of_the_right_account() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer(&mut joiner, "New-PC").await;
    sponsor
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.claim");
    joiner.conn.expect_notification("pairing.claimed").await;

    // Valid, unexpired — but issued too long ago to stand for a confirmation.
    let stale = env.oidc.id_token_with(TEST_SUB, |claims| {
        claims.insert("iat".into(), json!(unix_now() - 3600));
    });
    let refused = sponsor
        .conn
        .request(
            "pairing.approve",
            json!({ "pairing_id": &pairing_id, "id_token": stale, "bundle": BUNDLE }),
        )
        .await
        .expect_err("a stale token must not confirm");
    assert_eq!(refused.app_code(), "OIDC_INVALID");

    // Fresh, but for somebody else's account.
    let refused = sponsor
        .conn
        .request(
            "pairing.approve",
            json!({
                "pairing_id": &pairing_id,
                "id_token": env.oidc.id_token("someone-else"),
                "bundle": BUNDLE,
            }),
        )
        .await
        .expect_err("another account's token must not confirm");
    assert_eq!(refused.app_code(), "OIDC_INVALID");

    joiner.conn.assert_silent().await;
}

/// A device re-joining (enrolled, but holding no account key of its own) already
/// has an account. A sponsor from a DIFFERENT one answering it would be handing
/// over key material for an account the joiner is not in.
#[tokio::test]
async fn a_sponsor_from_another_account_cannot_answer() {
    let env = TestEnv::start().await;
    let mut rejoining = online_device(&env, "alice", "Alice-PC", "linux").await;
    let mut outsider = online_device(&env, "bob", "Bob-PC", "linux").await;

    let offered = rejoining
        .conn
        .request(
            "pairing.create",
            json!({
                "role": "joiner",
                "channel": JOINER_CHANNEL,
                "device": joining("Alice-PC", "linux", &rejoining.key.node_id()),
            }),
        )
        .await
        .expect("pairing.create");
    let pairing_id = offered["pairing_id"].as_str().expect("pairing_id");

    let refused = outsider
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect_err("accounts must not be bridged");
    assert_eq!(
        refused.app_code(),
        "PAIRING_UNKNOWN",
        "a foreign account is not even told the session exists"
    );
    rejoining.conn.assert_silent().await;
}

/// Declining is an answer too, and the other side deserves to hear it rather
/// than watch a spinner until the code expires.
#[tokio::test]
async fn declining_tells_the_other_side_and_settles_it() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer(&mut joiner, "New-PC").await;
    sponsor
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.claim");
    joiner.conn.expect_notification("pairing.claimed").await;

    sponsor
        .conn
        .request("pairing.cancel", json!({ "pairing_id": &pairing_id }))
        .await
        .expect("pairing.cancel");
    let failed = joiner.conn.expect_notification("pairing.failed").await;
    assert_eq!(failed["reason"], "declined");
    // The side that gave up has the response; it is not notified of its own
    // decision.
    sponsor.conn.assert_silent().await;

    let refused = joiner
        .enroll_with(&pairing_id)
        .await
        .expect_err("a declined pairing enrolls nothing");
    assert_eq!(refused.app_code(), "PAIRING_UNKNOWN");
}

/// A party that walks away cannot come back: the survivor is told at once, so a
/// closed window does not leave the other device waiting on nothing.
#[tokio::test]
async fn a_vanished_party_releases_the_other() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer(&mut joiner, "New-PC").await;
    sponsor
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.claim");

    drop(joiner);
    let failed = sponsor.conn.wait_notification("pairing.failed").await;
    assert_eq!(failed["reason"], "abandoned");
}

/// ...but a sponsor that has already confirmed may walk away: what is left of the
/// session is the joiner's own enrollment, on the joiner's own connection.
///
/// The window is real and short — a phone swiped out of the recents list right
/// after the confirmation kills its process — and the device it vouched for would
/// be left having installed the account key with no directory entry to go with
/// it, reporting a failure for something a human had legitimately approved.
#[tokio::test]
async fn an_approved_grant_outlives_the_sponsor_leaving() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    // A third device, for one reason only: it is the account's witness that the
    // server has finished handling the sponsor's departure. Without it nothing
    // would say when, and the enrollment below could pass by outrunning the
    // cleanup instead of surviving it.
    let mut witness = online_device(&env, TEST_SUB, "Desk-PC", "linux").await;
    let mut joiner = Joiner::arrive(&env).await;

    // The helper leaves the joiner having seen the confirmation: the bundle is
    // installed on its side, and only the enrollment is left.
    let pairing_id = offer_and_confirm(&env, &mut sponsor, &mut joiner, "New-PC").await;

    drop(sponsor);
    let offline = witness.conn.wait_notification("device.offline").await;
    assert!(offline["device_id"].is_string());

    let enrolled = joiner
        .enroll_with(&pairing_id)
        .await
        .expect("a confirmed device enrolls even if the sponsor has gone");
    assert_eq!(enrolled["device"]["name"], "New-PC");
    // And still exactly one shot: the grant is spent, not merely outliving its
    // sponsor.
    let refused = joiner
        .enroll_with(&pairing_id)
        .await
        .expect_err("a spent grant enrolls nothing");
    assert_eq!(refused.app_code(), "PAIRING_UNKNOWN");
}

/// The joiner is the party an approved session still needs: if IT leaves, the
/// grant goes with it. Nobody could spend it anyway — enrolling demands the very
/// connection that took part — and leaving it behind would hold a slot for
/// nothing.
#[tokio::test]
async fn an_approved_grant_dies_with_the_joiner() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer_and_confirm(&env, &mut sponsor, &mut joiner, "New-PC").await;

    // The same key, on a connection of its own: the seed is all it takes.
    let key = DeviceKey::from_seed_hex(&joiner.key.seed_hex());
    drop(joiner);
    // The sponsor is told, on the same grounds as before the confirmation: it was
    // waiting for nothing more, but a device that vanished mid-pairing is
    // something its interface has to be able to stop showing.
    let failed = sponsor.conn.wait_notification("pairing.failed").await;
    assert_eq!(failed["reason"], "abandoned");

    // A fresh connection with the same key cannot pick the grant up: the pairing
    // was pinned to the connection a human confirmed, not to the key.
    let mut second = env.connect().await;
    let nonce = challenge(&mut second).await;
    let refused = second
        .request(
            "auth.enroll",
            json!({ "pairing_id": &pairing_id, "proof": key.proof(&nonce) }),
        )
        .await
        .expect_err("an abandoned grant enrolls nothing");
    assert_eq!(refused.app_code(), "PAIRING_UNKNOWN");
}

/// One offer per connection: reopening the dialog must work, and the code left
/// on the abandoned screen must stop working.
#[tokio::test]
async fn a_new_offer_retires_the_connection_s_previous_one() {
    let env = TestEnv::start().await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;

    let abandoned = offer(&mut joiner, "New-PC").await;
    let current = offer(&mut joiner, "New-PC").await;
    assert_ne!(abandoned, current);

    let refused = sponsor
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &abandoned, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect_err("the retired code must be dead");
    assert_eq!(refused.app_code(), "PAIRING_UNKNOWN");
    sponsor
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &current, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("the current code still works");
}

/// A code left on a screen stops being one. Nothing is broadcast when it lapses:
/// both sides were told the lifetime up front and time out on their own clocks.
#[tokio::test]
async fn an_abandoned_code_expires() {
    let env = TestEnv::start_with(|config| {
        config.pairing_ttl = std::time::Duration::from_millis(150);
    })
    .await;
    let mut sponsor = online_device(&env, TEST_SUB, "Phone", "android").await;
    let mut joiner = Joiner::arrive(&env).await;
    let pairing_id = offer(&mut joiner, "New-PC").await;

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let refused = sponsor
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect_err("an expired code must not be claimable");
    assert_eq!(refused.app_code(), "PAIRING_UNKNOWN");
    joiner.conn.assert_silent().await;
}

/// Malformed calls answer with the plain JSON-RPC error, not an application
/// code: they are a client bug, not a state of the pairing.
#[tokio::test]
async fn malformed_pairing_calls_are_invalid_params() {
    let env = TestEnv::start().await;
    let mut joiner = Joiner::arrive(&env).await;

    let cases = [
        (
            "an unknown role",
            json!({ "role": "bystander", "channel": JOINER_CHANNEL }),
        ),
        (
            "no channel",
            json!({ "role": "joiner", "device": joining("PC", "linux", "ab") }),
        ),
        // A joiner is the device being confirmed: it has to say which one.
        (
            "a joiner without a device",
            json!({ "role": "joiner", "channel": JOINER_CHANNEL }),
        ),
        (
            "a platform outside the closed set",
            json!({
                "role": "joiner",
                "channel": JOINER_CHANNEL,
                "device": joining("PC", "toaster", "ab"),
            }),
        ),
    ];
    for (what, params) in cases {
        let refused = joiner
            .conn
            .request("pairing.create", params)
            .await
            .expect_err(what);
        assert_eq!(refused.code, -32602, "{what}: {refused:?}");
    }
}

// ---------------------------------------------------------------------------
// Shared prefixes of the flow.
// ---------------------------------------------------------------------------

/// The joiner opens a session and displays it → its id.
async fn offer(joiner: &mut Joiner, name: &str) -> String {
    let declaration = joiner.declaration(name);
    joiner
        .conn
        .request(
            "pairing.create",
            json!({
                "role": "joiner",
                "channel": JOINER_CHANNEL,
                "device": declaration,
            }),
        )
        .await
        .expect("pairing.create")["pairing_id"]
        .as_str()
        .expect("pairing_id")
        .to_string()
}

/// The whole flow up to the human's confirmation: the joiner displays, the
/// sponsor scans and confirms → the pairing id, now an enrollment grant.
async fn offer_and_confirm(
    env: &TestEnv,
    sponsor: &mut Device,
    joiner: &mut Joiner,
    name: &str,
) -> String {
    let pairing_id = offer(joiner, name).await;
    sponsor
        .conn
        .request(
            "pairing.claim",
            json!({ "pairing_id": &pairing_id, "channel": SPONSOR_CHANNEL }),
        )
        .await
        .expect("pairing.claim");
    sponsor
        .conn
        .request(
            "pairing.approve",
            json!({
                "pairing_id": &pairing_id,
                "id_token": env.oidc.id_token(TEST_SUB),
                "bundle": BUNDLE,
            }),
        )
        .await
        .expect("pairing.approve");
    joiner.conn.expect_notification("pairing.claimed").await;
    joiner.conn.expect_notification("pairing.completed").await;
    pairing_id
}
