// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! An account with no server: being IN the account and being LOGGED IN are two
//! different things, and only the first is what the trust root, the directory and
//! the data plane rest on (C7). This file pins that separation.
//!
//! It also pins what a record is worth once no server vouches for it: the device
//! SIGNS its own description, and the account can strike a device off with a
//! signature of its own (building block 2). What that buys is checked from both
//! ends — here for the gestures with no server at all, and in `revocation.rs` for
//! the property that matters most: a tombstone outlives what a server says.
//!
//! Every Core here is ALONE: what it knows, it knows from its own store. Two of
//! them telling each other whom they know is `dirsync.rs` (building block 3), and
//! the first introduction between two devices that have never met — the LAN
//! pairing — is still to come. Until it does, a serverless device joins the
//! account by having the recovery code typed into it, and every peer check stays
//! fail-closed.

use serde_json::json;
use universallink_core::{FileSecretStore, SecretStore};

use crate::support::*;

/// A GUI-shaped component: manages the account and the devices, reads the
/// directory.
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

/// The only record a serverless Core has: itself. Returned so each test can
/// assert on it rather than re-derive the list shape.
async fn only_device(c: &mut TestComponent) -> serde_json::Value {
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let devices = list.as_array().expect("a list").clone();
    assert_eq!(devices.len(), 1, "one device, itself: {devices:?}");
    devices[0].clone()
}

/// No server URL, no OIDC, no login — and still an account. This is the gesture
/// the whole serverless story starts from: `account.setup` used to demand a
/// connected server, which made "create an account without one" impossible.
#[tokio::test(flavor = "multi_thread")]
async fn an_account_is_created_with_no_server_at_all() {
    let core = TestCore::start().await;
    let mut c = manager(&core).await;

    // Nothing configured: no session is even conceivable.
    let session = c
        .request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(session["configured"], json!(false));
    assert_eq!(session["logged_in"], json!(false));

    let setup = c
        .request("account.setup", json!({}))
        .await
        .expect("account.setup with no server");
    assert!(
        setup["recovery_code"]
            .as_str()
            .is_some_and(|c| !c.is_empty()),
        "the code is the only copy of the account key: {setup}"
    );
    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true), "in the account");
    assert_eq!(status["holds_key"], json!(true), "and holding its key");
    assert_eq!(status["fingerprint"], setup["fingerprint"]);

    // And the directory answers, where it used to say SERVER_UNREACHABLE: a
    // device in the account knows at least itself.
    let own = only_device(&mut c).await;
    let node_id = core.node_id();
    assert_eq!(own["is_self"], json!(true));
    assert_eq!(own["node_id"], json!(node_id));
    assert_eq!(
        own["device_id"],
        json!(node_id),
        "no server named this device: its `device_id` is its own `node_id`"
    );
    assert_eq!(own["name"], json!(CORE_DEVICE_NAME));
    assert_eq!(own["platform"], json!(std::env::consts::OS));
    assert!(
        own["attestation"].as_str().is_some_and(|a| !a.is_empty()),
        "the record carries what proves it belongs: {own}"
    );
    // Its own liveness needs no server; a ROUTE to it still does (no relay
    // published, nobody seen on the LAN).
    assert_eq!(own["online"], json!(true));
    assert_eq!(own["lan"], json!(false));
    assert_eq!(own["reachable"], json!(false));
    // And the record stands on its own: signed by the very key its `node_id` is,
    // so a peer can take the description from anyone and still know who wrote it.
    assert!(
        own["seq"].as_u64().is_some_and(|seq| seq > 0),
        "an ordering token for its description: {own}"
    );
    assert!(
        universallink_core::directory::verify_record(&own),
        "the device signs what it says about itself: {own}"
    );
}

/// A description is signed once and kept: the Core writes it down, and a restart
/// hands the peers that already know this device the very record they hold — not a
/// new one saying something else under a fresh `seq`. Renamed first, deliberately:
/// a record re-minted at startup would carry the name this Core BOOTED with, which
/// is exactly how a rename would be quietly undone.
#[tokio::test(flavor = "multi_thread")]
async fn the_description_a_device_signed_survives_a_restart() {
    let core = TestCore::start().await;
    let mut c = manager(&core).await;
    c.request("account.setup", json!({}))
        .await
        .expect("account.setup with no server");
    let before = c
        .request(
            "devices.rename",
            json!({ "device_id": core.node_id(), "name": "Atelier" }),
        )
        .await
        .expect("devices.rename")["device"]
        .clone();

    let core = core.restart().await;
    let mut c = manager(&core).await;

    let after = only_device(&mut c).await;
    assert_eq!(after["name"], json!("Atelier"), "not re-minted");
    assert_eq!(after["seq"], before["seq"]);
    assert_eq!(after["self_sig"], before["self_sig"]);
    assert!(universallink_core::directory::verify_record(&after));
    assert_eq!(after["online"], json!(true), "and it is running");
}

/// Renaming needs no server: this device is the authority on its own name. The
/// new description carries a higher `seq` — which is what makes a peer prefer it
/// over the one it already holds — and a signature over that name.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_renames_itself_with_no_server() {
    let code = universallink_core::account_key::generate_recovery_code();
    let core = TestCore::start_in_account(&code).await;
    let mut c = manager(&core).await;
    c.request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .expect("events.subscribe");
    let before = only_device(&mut c).await;

    let renamed = c
        .request(
            "devices.rename",
            json!({ "device_id": core.node_id(), "name": "Atelier" }),
        )
        .await
        .expect("devices.rename with no server")["device"]
        .clone();

    assert_eq!(renamed["name"], json!("Atelier"));
    assert_eq!(renamed["is_self"], json!(true));
    assert!(
        renamed["seq"].as_u64() > before["seq"].as_u64(),
        "{renamed} must supersede {before}"
    );
    assert!(
        universallink_core::directory::verify_record(&renamed),
        "and be signed under the new name: {renamed}"
    );
    // Told to whoever subscribed, not just to the caller.
    let event = c.expect_notification("device.updated").await;
    assert_eq!(event["device"]["name"], json!("Atelier"));
    assert_eq!(only_device(&mut c).await, renamed);

    // And it is the name that comes back, with the same signature: a rename is a
    // decision, not a runtime nicety.
    let core = core.restart().await;
    let mut c = manager(&core).await;
    let after = only_device(&mut c).await;
    assert_eq!(after["name"], json!("Atelier"));
    assert_eq!(after["seq"], renamed["seq"]);
    assert_eq!(after["self_sig"], renamed["self_sig"]);
}

/// Another device's description is signed by that device: renaming it means
/// asking it, and the only thing that carries such an ask is the server. So the
/// answer stays exactly what it was before this path existed.
#[tokio::test(flavor = "multi_thread")]
async fn renaming_another_device_still_needs_a_server() {
    let code = universallink_core::account_key::generate_recovery_code();
    let sibling = DeviceKey::generate();
    let core = TestCore::start_in_account_holding(
        &code,
        &[peer_record(&sibling, "Old-Laptop", "linux", &code, 1)],
    )
    .await;
    let mut c = manager(&core).await;

    let refused = c
        .request(
            "devices.rename",
            json!({ "device_id": sibling.node_id(), "name": "Not-Mine" }),
        )
        .await
        .expect_err("not ours to sign");
    assert_eq!(refused.app_code(), "SERVER_UNREACHABLE");
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(
        find_device(&list, &sibling.node_id())["name"],
        json!("Old-Laptop"),
        "and nothing changed locally either"
    );
}

/// A serverless account is not a runtime accident: it comes back from disk, from
/// `account-key.json` alone — there is no `session.json` to hang it on.
#[tokio::test(flavor = "multi_thread")]
async fn the_account_survives_a_restart_with_no_server() {
    let code = universallink_core::account_key::generate_recovery_code();
    let core = TestCore::start_in_account(&code).await;
    let mut c = manager(&core).await;
    let before = only_device(&mut c).await;

    let core = core.restart().await;
    let mut c = manager(&core).await;

    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true), "still in the account");
    let after = only_device(&mut c).await;
    assert_eq!(after, before, "the same device, told the same way");
}

/// The directory cache expires because a server could refresh it. With no server
/// there is nothing to be stale with respect to, and expiring the file would not
/// fail closed — it would erase the account's other devices for good.
#[tokio::test(flavor = "multi_thread")]
async fn a_serverless_directory_does_not_expire() {
    let code = universallink_core::account_key::generate_recovery_code();
    let core = TestCore::start_in_account(&code).await;

    // A sibling of the same account — attested under the same key, and signing its
    // own description, as its own device would — written into a store far older
    // than the cache TTL.
    let sibling = DeviceKey::generate();
    seed_directory(
        core.config_dir(),
        &[peer_record(&sibling, "Old-Laptop", "linux", &code, 1)],
        90 * 24 * 60 * 60,
    );

    let core = core.restart().await;
    let mut c = manager(&core).await;

    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    let devices = list.as_array().expect("a list");
    assert_eq!(
        devices.len(),
        2,
        "the sibling from the store, plus ourselves: {devices:?}"
    );
    let peer = find_device(&list, &sibling.node_id());
    assert_eq!(peer["name"], json!("Old-Laptop"));
    assert_eq!(peer["is_self"], json!(false));
}

/// The gate that was loosened must still hold where it was written for: a
/// deployment. Joining an account publishes an attestation the other devices read
/// from the server, and a server that is configured but unreachable cannot carry
/// it — so the device would be in the account for itself alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_server_that_is_down_still_refuses_to_create_an_account() {
    let server = TestServer::start().await;
    // Configured, never logged in: there is a server, and no connection to it.
    let core = TestCore::start_with_server(&server).await;
    let mut c = manager(&core).await;

    let refused = c
        .request("account.setup", json!({}))
        .await
        .expect_err("no server connection, no account");
    assert_eq!(refused.app_code(), "SERVER_UNREACHABLE");
    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(false));
}

/// The continuum, the easy half: an account born with no server can be pointed at
/// one afterwards. The device enrolls, the server names it — and it must not end
/// up appearing in its own directory twice, or as a stranger under the label it
/// minted for itself (`state::rekey_own`, whose exact effect is pinned by a unit
/// test — here it is the end state that matters).
#[tokio::test(flavor = "multi_thread")]
async fn a_serverless_account_can_be_pointed_at_a_server_afterwards() {
    let core = TestCore::start().await;
    let mut c = manager(&core).await;
    c.request("account.setup", json!({}))
        .await
        .expect("account.setup with no server");
    let fingerprint = c
        .request("account.status", json!({}))
        .await
        .expect("account.status")["fingerprint"]
        .clone();

    // The user points this device at a deployment — the GUI writes `config.json`,
    // the Core re-reads it — then logs in.
    let server = TestServer::start().await;
    core.stage_config(Some(server_cfg(&server)));
    let status = c
        .request("session.reload", json!({}))
        .await
        .expect("session.reload");
    assert_eq!(status["configured"], json!(true));
    complete_login(&mut c).await;
    wait_server_connected(&mut c, true).await;

    let own = only_device(&mut c).await;
    assert_eq!(own["is_self"], json!(true), "itself, not a stranger");
    assert_ne!(
        own["device_id"],
        json!(core.node_id()),
        "the self-minted label gave way to the server's"
    );
    assert_eq!(
        own["node_id"],
        json!(core.node_id()),
        "same crypto identity"
    );
    // The account did not move: same key, same fingerprint, and the attestation
    // the server now carries is the one derived from the code that never left
    // this machine.
    let after = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(after["fingerprint"], fingerprint);
    assert!(own["attestation"].as_str().is_some_and(|a| !a.is_empty()));
}

/// "Nothing configured" is not "no server": a session carries its own server URL,
/// so a Core whose `config.json` went missing still answers to one — and so does
/// every other device of the account. The local-only paths must not take over
/// there, or this device would sign a name nobody else would ever hear about.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_without_a_configured_server_is_still_a_server() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled_without_config(&server).await;
    let mut c = manager(&core).await;
    wait_server_connected(&mut c, true).await;
    // Another device of the account, watching: it is the server that tells it, so
    // it hears nothing at all if this rename stayed local.
    let mut other = server.online_device("Other-PC", "linux").await;

    let renamed = c
        .request(
            "devices.rename",
            json!({ "device_id": core.device_id(), "name": "Atelier" }),
        )
        .await
        .expect("devices.rename through the session")["device"]
        .clone();

    assert_eq!(renamed["name"], json!("Atelier"));
    let event = other.conn.wait_notification("device.updated").await;
    assert_eq!(event["device"]["name"], json!("Atelier"));
    assert_eq!(event["device"]["device_id"], json!(core.device_id()));
}

/// Striking a device off with no server to strike it from: the account key signs
/// a tombstone, and that is what keeps the device out afterwards — not the record
/// being gone. The proof is to put the record back and start again.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_is_struck_off_for_good_with_no_server() {
    let code = universallink_core::account_key::generate_recovery_code();
    let lost = DeviceKey::generate();
    let lost_record = peer_record(&lost, "Lost-Phone", "android", &code, 1);
    let core = TestCore::start_in_account_holding(&code, std::slice::from_ref(&lost_record)).await;
    let mut c = manager(&core).await;
    c.request("events.subscribe", json!({ "topics": ["devices"] }))
        .await
        .expect("events.subscribe");
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(list.as_array().map(Vec::len), Some(2), "the two of them");

    let done = c
        .request("devices.revoke", json!({ "device_id": lost.node_id() }))
        .await
        .expect("devices.revoke with no server");

    assert_eq!(done["status"], json!("done"));
    let event = c.expect_notification("device.removed").await;
    assert_eq!(event["device_id"], json!(lost.node_id()));
    let own = only_device(&mut c).await;
    assert_eq!(own["is_self"], json!(true), "only ourselves left");

    // The record put back by hand, as a sibling's roster or a stale backup would
    // put it back: the tombstone is what refuses it, and it is signed by a key no
    // amount of directory editing produces.
    seed_directory(core.config_dir(), &[lost_record, own], 0);
    let core = core.restart().await;
    let mut c = manager(&core).await;
    let after = only_device(&mut c).await;
    assert_eq!(after["is_self"], json!(true), "struck off is struck off");
}

/// The account strikes a device off, not this machine: without the account's
/// PRIVATE key there is nothing to sign the tombstone with, and a device that
/// holds the account without its key must say so rather than pretend.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_cannot_sign_cannot_strike_anyone_off() {
    let code = universallink_core::account_key::generate_recovery_code();
    let lost = DeviceKey::generate();
    let core = TestCore::start_in_account_holding(
        &code,
        &[peer_record(&lost, "Lost-Phone", "android", &code, 1)],
    )
    .await;
    // The state `holds_key: false` describes: in the account, keyring emptied.
    FileSecretStore::new(core.config_dir()).delete("account-key-seed");
    let mut c = manager(&core).await;

    let refused = c
        .request("devices.revoke", json!({ "device_id": lost.node_id() }))
        .await
        .expect_err("nothing to sign with");

    assert_eq!(refused.app_code(), "NO_ACCOUNT_KEY");
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    assert_eq!(
        list.as_array().map(Vec::len),
        Some(2),
        "and the device is still there: {list}"
    );
}

/// Striking OURSELVES off would bar this installation from its own account with
/// no way back — a tombstone cannot be withdrawn. Leaving the account is a logout
/// plus erasing the trust root, not a signature against oneself.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_does_not_strike_itself_off() {
    let code = universallink_core::account_key::generate_recovery_code();
    let core = TestCore::start_in_account(&code).await;
    let mut c = manager(&core).await;

    let refused = c
        .request("devices.revoke", json!({ "device_id": core.node_id() }))
        .await
        .expect_err("not against oneself");
    assert_eq!(refused.app_code(), "CANNOT_REVOKE_SELF");

    // A device it never knew is not a device it can strike off either: there is
    // no `node_id` to sign about.
    let refused = c
        .request("devices.revoke", json!({ "device_id": "d_nobody" }))
        .await
        .expect_err("unknown device");
    assert_eq!(refused.app_code(), "DEVICE_UNKNOWN");

    let own = only_device(&mut c).await;
    assert_eq!(own["is_self"], json!(true), "still itself, still in");
}

/// Logging out ends a session, not a membership: the trust root stays (a
/// re-enrollment or a pairing rests on it), so what this device knows first-hand
/// about itself stays too — under the self-minted `device_id`, the server's label
/// having left with the session.
#[tokio::test(flavor = "multi_thread")]
async fn logging_out_leaves_the_account_knowing_itself() {
    let server = TestServer::start().await;
    let core = TestCore::start_with_server(&server).await;
    let mut c = manager(&core).await;
    complete_login(&mut c).await;
    c.request("account.setup", json!({}))
        .await
        .expect("account.setup");

    c.request("session.logout", json!({}))
        .await
        .expect("session.logout");

    let status = c
        .request("account.status", json!({}))
        .await
        .expect("account.status");
    assert_eq!(status["attested"], json!(true), "still in the account");
    let own = only_device(&mut c).await;
    assert_eq!(own["is_self"], json!(true), "and still recognizes itself");
    assert_eq!(own["device_id"], json!(core.node_id()));
    assert_eq!(own["node_id"], json!(core.node_id()));
}
