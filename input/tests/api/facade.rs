// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The `input.*` gestures, against a real Core and a real engine: what an
//! interface can ask for, what it gets back, and what it is refused
//! (doc/input-sharing.md, section 12).
//!
//! Every assertion here reads the AUTHORITATIVE snapshot rather than a gesture's
//! reply body, on purpose: `input.status` is the state and the notifications
//! merely echo it, so a test that believed a reply and never looked at the
//! snapshot would pass on an engine that answered politely and stored nothing.

use serde_json::json;

use crate::support::*;

/// The snapshot answers from the first moment the engine serves, and it carries
/// this machine's own identity, its screens and whether it could drive or be
/// driven. An interface that opens late has to be able to render everything from
/// this one call.
#[tokio::test(flavor = "multi_thread")]
async fn the_snapshot_is_the_whole_state_and_it_answers_from_the_start() {
    let fleet = Fleet::start().await;
    let status = fleet.a.status().await;

    assert_eq!(
        status["here"]["device_id"],
        json!(fleet.a.device_id()),
        "the snapshot says which computer it is speaking for"
    );
    // The fake backend can do everything, so this machine says so.
    assert_eq!(status["here"]["can_drive"], json!(true));
    assert_eq!(status["here"]["can_be_driven"], json!(true));
    assert_eq!(
        status["here"]["problem"],
        json!(null),
        "a backend that works reports no problem"
    );
    assert!(
        status["plane"]["id"]
            .as_str()
            .is_some_and(|id| id.len() == 32),
        "the plane always has an id, even before anyone has dragged anything: {status:#}"
    );

    // And the other computer of the account is in the list, with both of its
    // switches off: a peer nobody has said anything about can neither type here
    // nor be typed on from here.
    let row = peer_row(&status, fleet.b.device_id()).expect("B is in the list");
    assert_eq!(row["allowed"], json!(false));
    assert_eq!(row["drive"], json!(false));
    assert_eq!(row["rtt_ms"], json!(null), "nothing has been measured yet");
}

/// The two switches are two separate answers, they survive in the snapshot, and
/// **the one that is the authority is not visible from the other side**. That
/// last assertion is the doctrine: a grant is stored on the machine that will be
/// driven and is never replicated, so B's own snapshot must not learn that A has
/// allowed it.
#[tokio::test(flavor = "multi_thread")]
async fn allowing_and_driving_are_stored_here_and_never_replicated() {
    let fleet = Fleet::start().await;

    fleet.a.allow(&fleet.b, true).await;
    let row = fleet
        .a
        .until_peer(fleet.b.device_id(), "A to record the grant", |row| {
            row["allowed"] == json!(true)
        })
        .await;
    assert_eq!(
        row["drive"],
        json!(false),
        "letting a computer type here says nothing about typing on it"
    );

    // B is told nothing. It may be driven by A, and its own view of A says only
    // what B itself has decided.
    let b_row = peer_row(&fleet.b.status().await, fleet.a.device_id())
        .cloned()
        .expect("A is in B's list");
    assert_eq!(
        b_row["allowed"],
        json!(false),
        "a grant is never replicated: B has allowed nobody"
    );
    assert_eq!(
        b_row["drive"],
        json!(false),
        "and B has not said it wants to drive A either"
    );

    // Withdrawing it is just as visible, and just as local.
    fleet.a.allow(&fleet.b, false).await;
    fleet
        .a
        .until_peer(fleet.b.device_id(), "A to withdraw the grant", |row| {
            row["allowed"] == json!(false)
        })
        .await;
}

/// Every gesture refuses a device the account does not have, and a malformed
/// request is a real protocol error rather than an application state dressed up
/// as one.
#[tokio::test(flavor = "multi_thread")]
async fn a_gesture_refuses_a_stranger_and_a_malformed_call_is_minus_32602() {
    let fleet = Fleet::start().await;
    let stranger = "d_not_of_this_account";

    for (method, params) in [
        (
            "input.allow",
            json!({ "device_id": stranger, "allowed": true }),
        ),
        (
            "input.drive",
            json!({ "device_id": stranger, "allowed": true }),
        ),
        ("input.take", json!({ "device_id": stranger })),
        ("input.remap", json!({ "device_id": stranger, "map": {} })),
    ] {
        assert_eq!(
            fleet.a.ui.refusal(method, params).await,
            "INPUT_DEVICE_UNKNOWN",
            "{method} must refuse a device this account does not have"
        );
    }

    // Shape, not state: the engine emits the genuine JSON-RPC code.
    for (method, params) in [
        ("input.allow", json!({})),
        ("input.allow", json!({ "device_id": 7, "allowed": true })),
        ("input.drive", json!({ "device_id": stranger })),
        ("input.lock", json!({})),
        ("input.hotkey", json!({ "keys": "ctrl+alt+escape" })),
        ("input.place", json!({})),
    ] {
        let code = fleet.a.ui.refusal(method, params.clone()).await;
        assert_eq!(
            code, "code -32602",
            "{method} {params} is a malformed request, not an application state"
        );
    }
}

/// The guards are per neighbour, they survive in the snapshot, and an absurd
/// value is bounded rather than obeyed: a dwell of an hour is a pointer that
/// never crosses and a human who cannot tell why.
#[tokio::test(flavor = "multi_thread")]
async fn the_guards_are_stored_per_neighbour_and_bounded() {
    let fleet = Fleet::start().await;
    // Arranged rather than merely published: the side a crossing is on has to be
    // a fact of the test rather than of which node_id happened to sort first.
    fleet
        .arranged(
            vec![screen("A1", 0, 0, 1920, 1080)],
            vec![screen("B1", 0, 0, 1920, 1080)],
        )
        .await;

    let mine = format!("{}/A1", fleet.a.node_id());
    let theirs = format!("{}/B1", fleet.b.node_id());
    fleet
        .a
        .ui
        .request(
            "input.guards",
            json!({ "device_id": fleet.b.device_id(), "monitor": mine,
                    "side": "right",
                    "guards": { "dwell_ms": 3_600_000u64, "double_tap_ms": 200,
                                "dead_corner": 4, "wall": false } }),
        )
        .await
        .expect("input.guards");

    let status = fleet.a.status().await;
    let guards = status["guards"]
        .as_array()
        .expect("the snapshot carries the guards a human set");
    // The snapshot names the NEIGHBOUR's screen, whichever end the gesture named:
    // a guard is about a crossing, and the crossing is identified by where it
    // goes. `input.guards` takes either end back, which is why the gesture above
    // could name ours.
    let entry = guards
        .iter()
        .find(|g| g["monitor"] == json!(theirs))
        .unwrap_or_else(|| panic!("the guard is in the snapshot: {status:#}"));
    assert_eq!(entry["device_id"], json!(fleet.b.device_id()));
    assert_eq!(entry["side"], json!("right"));
    assert!(
        entry["dwell_ms"].as_u64().is_some_and(|ms| ms <= 10_000),
        "an absurd dwell is bounded, not obeyed: {entry:#}"
    );
    assert_eq!(entry["double_tap_ms"], json!(200));
    assert_eq!(entry["dead_corner"], json!(4));

    // A guard naming a monitor no record claims is refused: a setting that can
    // never apply is a setting that lies to the interface.
    assert_eq!(
        fleet
            .a
            .ui
            .refusal(
                "input.guards",
                json!({ "device_id": fleet.b.device_id(),
                        "monitor": format!("{}/NOPE", fleet.a.node_id()),
                        "side": "right", "guards": {} }),
            )
            .await,
        "INPUT_UNKNOWN_MONITOR"
    );
    // And so is a side with no crossing on it: nothing is across A's left edge.
    assert_eq!(
        fleet
            .a
            .ui
            .refusal(
                "input.guards",
                json!({ "device_id": fleet.b.device_id(), "monitor": theirs,
                        "side": "left", "guards": {} }),
            )
            .await,
        "INPUT_UNKNOWN_MONITOR"
    );
}

/// The lock and the hotkey are this machine's own preferences: they survive in
/// the snapshot, and a hotkey this build cannot parse is refused rather than
/// stored as something that would never fire.
#[tokio::test(flavor = "multi_thread")]
async fn the_lock_and_the_hotkey_are_local_preferences() {
    let fleet = Fleet::start().await;

    let status = fleet.a.status().await;
    assert_eq!(status["lock"], json!(false));
    assert_eq!(
        status["hotkey"],
        json!(["ctrl", "alt", "Escape"]),
        "the default is Ctrl + Alt + Escape, the one chord free on all three desktops"
    );

    fleet
        .a
        .ui
        .request("input.lock", json!({ "locked": true }))
        .await
        .expect("input.lock");
    fleet
        .a
        .until("the lock to be recorded", |status| {
            status["lock"] == json!(true)
        })
        .await;

    fleet
        .a
        .ui
        .request("input.hotkey", json!({ "keys": ["ctrl", "shift", "Home"] }))
        .await
        .expect("input.hotkey");
    // Read back in PRESENTATION order, not the order it was given in: a chord is
    // a set plus a key, so one binding must have one spelling, and the spelling is
    // the one all three desktops use (Control, the alternate keys, Shift, then the
    // platform key). Here that happens to be the order it was given in; the test
    // below proves the order given does not matter.
    fleet
        .a
        .until("the hotkey to be recorded", |status| {
            status["hotkey"] == json!(["ctrl", "shift", "Home"])
        })
        .await;
    // Given the other way round, it comes back the same way.
    fleet
        .a
        .ui
        .request("input.hotkey", json!({ "keys": ["Home", "shift", "ctrl"] }))
        .await
        .expect("a chord is a set: its order is the caller's business, not ours");
    assert_eq!(
        fleet.a.status().await["hotkey"],
        json!(["ctrl", "shift", "Home"])
    );

    // A chord with no modifier would swallow every one of those keys a session
    // sees; a name this build does not know would never fire at all.
    for keys in [
        json!(["Escape"]),
        json!(["ctrl", "alt", "Nonsense"]),
        json!(["ctrl", "alt"]),
        json!([]),
    ] {
        let code = fleet
            .a
            .ui
            .refusal("input.hotkey", json!({ "keys": keys.clone() }))
            .await;
        assert_eq!(
            code, "code -32602",
            "{keys} is not a hotkey this engine can enforce"
        );
    }
    // And the one that was accepted is still the one in force.
    assert_eq!(
        fleet.a.status().await["hotkey"],
        json!(["ctrl", "shift", "Home"]),
        "a refused hotkey must not have replaced the one that works"
    );
}

/// Everything a human decides survives a restart of the engine, because the Core
/// stores nothing about input and the engine's own files are the only record.
#[tokio::test(flavor = "multi_thread")]
async fn what_a_human_decided_survives_the_engine_restarting() {
    let fleet = Fleet::start().await;
    let mut fleet = fleet;
    fleet.a.allow(&fleet.b, true).await;
    fleet.a.drive(&fleet.b, true).await;
    fleet
        .a
        .ui
        .request("input.lock", json!({ "locked": true }))
        .await
        .expect("input.lock");
    fleet
        .a
        .until_peer(fleet.b.device_id(), "the switches to be recorded", |row| {
            row["allowed"] == json!(true) && row["drive"] == json!(true)
        })
        .await;

    // Stopped the way the supervisor stops it, then started again on the same
    // state directory.
    assert_eq!(
        fleet.a.stop_engine().await,
        onedevice_input::Outcome::StdinClosed,
        "the engine stops cleanly when its standard input closes"
    );
    let restarted = Device::start(fleet.a.core).await;

    let row = restarted
        .until_peer(fleet.b.device_id(), "the switches to come back", |row| {
            row["allowed"] == json!(true) && row["drive"] == json!(true)
        })
        .await;
    assert_eq!(row["allowed"], json!(true));
    assert_eq!(
        restarted.status().await["lock"],
        json!(true),
        "the lock is a preference and it survives too"
    );
}
