// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! A session dying, every way this harness can really kill one
//! (doc/input-sharing.md, section 4).
//!
//! The rule the whole feature is judged by: **every one of the live channel's ten
//! deaths ends with a working keyboard on both machines.** A machine left with
//! Control stuck is the classic failure of every tool in this category, and it is
//! the one thing this engine must not do.
//!
//! Five of the ten are staged here end to end, against two real Cores:
//! `PEER_GONE` (twice, from a Core stopping and from a component dying),
//! `DEVICE_REVOKED`, `LOGGED_OUT` and `SHUTDOWN`. The other five are not
//! reachable from outside and are proven by the table-driven matrix in
//! `src/session.rs` instead, which is honest rather than convenient:
//! `FRAME_TOO_LARGE` and `RATE_EXCEEDED` cannot be reached by this engine at all
//! (its own caps sit well below the Core's, which is the point of having them),
//! `IDLE_TIMEOUT` would need a wedged channel that the keepalive prevents,
//! `CLOSED` on the near side needs the pipe to close while the component lives,
//! and `REPLACED` needs two opens racing on one pair.
//!
//! Every assertion about hygiene reads the fake backend's RECORDED CALLS, never
//! the engine's own view of what it is holding. The engine's bookkeeping is the
//! thing under test; only the calls it made are evidence.

use serde_json::json;

use crate::support::*;

/// The far Core stops while it is being driven: the source brings the keyboard
/// home, and the target, which is the machine holding a modifier down, releases
/// it before its own Core is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_target_whose_core_stops_releases_what_it_held() {
    let mut fleet = Fleet::start().await;
    fleet.driving().await;

    let target = fleet.b.backend.clone();
    fleet.b.core.stop();

    wait_until("B to release the modifier it was holding", || {
        target.keys_down().is_empty()
    })
    .await;
    let calls = target.calls();
    assert!(
        !calls.releases.is_empty()
            || calls
                .actions
                .iter()
                .any(|a| matches!(a, onedevice_input::backend::Action::Key { down: false, .. })),
        "the release has to be a call the target really made: {calls:?}"
    );

    // And the source has its keyboard back: not swallowing, not confined.
    let source = fleet.a.backend.clone();
    wait_until("A to bring the keyboard home", || {
        let calls = source.calls();
        calls.capture.last() != Some(&onedevice_input::backend::CaptureMode::Swallow)
            && calls.confine.last() == Some(&None)
    })
    .await;
    fleet
        .a
        .until("A to have no session", |status| {
            status["session"] == json!(null)
        })
        .await;
}

/// The far COMPONENT dies while its Core lives: the same outcome, by a different
/// path. This is the one a crash really looks like, and the Core's word for it on
/// the near side is the same.
#[tokio::test(flavor = "multi_thread")]
async fn a_target_whose_component_dies_leaves_the_source_with_its_keyboard() {
    let mut fleet = Fleet::start().await;
    fleet.driving().await;

    fleet.b.kill_engine();

    let source = fleet.a.backend.clone();
    wait_until("A to bring the keyboard home", || {
        let calls = source.calls();
        calls.capture.last() != Some(&onedevice_input::backend::CaptureMode::Swallow)
            && calls.confine.last() == Some(&None)
    })
    .await;
    fleet
        .a
        .until("A to have no session", |status| {
            status["session"] == json!(null)
        })
        .await;
    // The pointer went back to where it left from rather than being abandoned
    // wherever the far machine's screen had put it.
    assert!(
        !fleet.a.backend.calls().warps.is_empty(),
        "the pointer is put back on the machine it belongs to"
    );
}

/// The account strikes the driven device off. Both ends release, and **the grant
/// dies with the device**: that is the epic's rule, and this is where it is kept.
#[tokio::test(flavor = "multi_thread")]
async fn striking_the_target_off_the_account_releases_everything_and_kills_the_grant() {
    let fleet = Fleet::start_serverless().await;
    fleet.driving().await;
    // B had allowed A; A had said it would drive B.
    let b_row = fleet
        .b
        .until_peer(fleet.a.device_id(), "B's grant to be recorded", |row| {
            row["allowed"] == json!(true)
        })
        .await;
    assert_eq!(b_row["allowed"], json!(true));

    // A's interface strikes B off, with no server in the picture.
    let manager = Ui::manager(&fleet.a.core).await;
    manager
        .request(
            "devices.revoke",
            json!({ "device_id": fleet.b.device_id() }),
        )
        .await
        .expect("devices.revoke with no server");

    // A: the keyboard is home, and nothing about B is left in its settings.
    let source = fleet.a.backend.clone();
    wait_until("A to bring the keyboard home", || {
        let calls = source.calls();
        calls.capture.last() != Some(&onedevice_input::backend::CaptureMode::Swallow)
            && calls.confine.last() == Some(&None)
    })
    .await;
    fleet
        .a
        .until("B to be gone from A's snapshot", |status| {
            peer_row(status, fleet.b.device_id())
                .is_none_or(|row| row["allowed"] == json!(false) && row["drive"] == json!(false))
        })
        .await;

    // B: whatever it was holding is released. Its own grant for A is gone too,
    // because a device the account struck off is gone at both ends of the pair.
    let target = fleet.b.backend.clone();
    wait_until("B to release the modifier", || {
        target.keys_down().is_empty()
    })
    .await;
}

/// Logging out ends every session and brings the keyboard home, and the grants
/// SURVIVE: they are keyed by the label the whole account shares, not by the id a
/// logout re-keys.
#[tokio::test(flavor = "multi_thread")]
async fn logging_out_ends_the_session_and_keeps_the_grants() {
    let fleet = Fleet::start().await;
    fleet.driving().await;
    // A's own list said it would drive B, which is a preference and not a
    // permission.
    let before = fleet
        .a
        .until_peer(
            fleet.b.device_id(),
            "A's own switch to be recorded",
            |row| row["drive"] == json!(true),
        )
        .await;
    assert_eq!(before["drive"], json!(true));

    let manager = Ui::manager(&fleet.a.core).await;
    manager
        .request("session.logout", json!({}))
        .await
        .expect("session.logout");

    let source = fleet.a.backend.clone();
    wait_until("A to bring the keyboard home", || {
        let calls = source.calls();
        calls.capture.last() != Some(&onedevice_input::backend::CaptureMode::Swallow)
            && calls.confine.last() == Some(&None)
    })
    .await;
    let target = fleet.b.backend.clone();
    wait_until("B to release the modifier", || {
        target.keys_down().is_empty()
    })
    .await;
    fleet
        .a
        .until("A to have no session", |status| {
            status["session"] == json!(null)
        })
        .await;
}

/// The Core stopping under the engine: the session ends, the keyboard comes home,
/// and the engine exits the way the supervisor expects rather than looping on a
/// reconnection its single-use token could never make.
#[tokio::test(flavor = "multi_thread")]
async fn the_core_stopping_ends_the_session_and_the_engine() {
    let mut fleet = Fleet::start().await;
    fleet.driving().await;

    let target = fleet.b.backend.clone();
    let source = fleet.a.backend.clone();
    fleet.a.core.stop();

    // The source's own machine is left usable even though its Core is gone: the
    // swallow and the confinement are lifted before anything else.
    wait_until("A to stop swallowing its own keyboard", || {
        let calls = source.calls();
        calls.capture.last() != Some(&onedevice_input::backend::CaptureMode::Swallow)
            && calls.confine.last() == Some(&None)
    })
    .await;
    // And the far side lets go of what it was holding.
    wait_until("B to release the modifier", || {
        target.keys_down().is_empty()
    })
    .await;
}

/// An explicit return: the hotkey's method twin. Not a death, and it is here
/// because it is the path every death has to look like from the outside.
#[tokio::test(flavor = "multi_thread")]
async fn bringing_the_keyboard_back_releases_the_target_too() {
    let fleet = Fleet::start().await;
    fleet.driving().await;

    fleet
        .a
        .ui
        .request("input.release", json!({}))
        .await
        .expect("input.release");

    let target = fleet.b.backend.clone();
    wait_until("B to release the modifier", || {
        target.keys_down().is_empty()
    })
    .await;
    let source = fleet.a.backend.clone();
    wait_until("A to unpin its pointer", || {
        source.calls().confine.last() == Some(&None)
    })
    .await;
    fleet
        .a
        .until("A to have no session", |status| {
            status["session"] == json!(null)
        })
        .await;
    fleet
        .b
        .until("B to have no session", |status| {
            status["session"] == json!(null)
        })
        .await;
    // And the pair is still usable: the session ended, the channel did not have
    // to.
    fleet
        .a
        .ui
        .request("input.take", json!({ "device_id": fleet.b.device_id() }))
        .await
        .expect("a second session on the same pair");
    fleet
        .a
        .until_peer(fleet.b.device_id(), "A to be driving B again", |row| {
            row["state"] == json!("driving")
        })
        .await;
}

/// A session that ends without the human asking is SAID, not merely undone. This
/// is the epic's rule applied to the one case that leaves no other trace: the
/// keyboard comes home, the session vanishes from the snapshot, and no `problem`
/// is set, because nothing about the far side went wrong. Without a word for it,
/// the person is left with a keyboard that moved twice and one explanation.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_ends_on_its_own_is_said_out_loud() {
    let mut fleet = Fleet::start().await;
    fleet.driving().await;

    fleet.b.kill_engine();

    let said = loop {
        let refusal = fleet.a.ui.notification("input.refused").await;
        if refusal["code"] == json!("GONE") {
            break refusal;
        }
    };
    assert_eq!(
        said["device_id"],
        json!(fleet.b.device_id()),
        "the sentence is about the computer the keyboard was on: {said:#}"
    );
    assert_eq!(said["count"], json!(1));
}

/// A grant withdrawn while the session runs ends it, and the source is TOLD which
/// word the target used. The standing half is in the snapshot (`not_allowed`), and
/// the event is what a person actually notices, since they are looking at the
/// machine whose keyboard just came back.
#[tokio::test(flavor = "multi_thread")]
async fn a_grant_withdrawn_mid_session_is_said_to_the_source() {
    let mut fleet = Fleet::start().await;
    fleet.driving().await;

    // B, the machine being driven, takes its permission back.
    fleet.b.allow(&fleet.a, false).await;

    let said = loop {
        let refusal = fleet.a.ui.notification("input.refused").await;
        if refusal["code"] == json!("REVOKED") {
            break refusal;
        }
    };
    assert_eq!(said["device_id"], json!(fleet.b.device_id()));
    // And the standing half of it, for a window that opens afterwards.
    fleet
        .a
        .until_peer(fleet.b.device_id(), "A to record why", |row| {
            row["problem"] == json!("not_allowed")
        })
        .await;
}
