// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The plane, between two real devices over `peers.send`
//! (doc/input-sharing.md, section 6).
//!
//! The unit tests in `src/plane.rs` prove the merge is deterministic against
//! documents built by hand. What this file proves is the part they cannot: that
//! two engines, each publishing its own screens and each merging the other's
//! rounds through a real Core, arrive at the SAME plane and the same plane id,
//! with nobody coordinating them. That agreement is what makes an absolute
//! pointer position mean one thing, so it is the foundation everything else
//! stands on.

use serde_json::json;

use crate::support::*;

/// Each device publishes its own screens, both learn the other's, and both
/// compute the same plane id. If this ever fails, every session between the two
/// is refused `PLANE_STALE` for ever with nothing able to repair it.
#[tokio::test(flavor = "multi_thread")]
async fn two_devices_converge_on_one_plane_with_nobody_coordinating() {
    let fleet = Fleet::start().await;
    fleet
        .screens(
            vec![screen("A1", 0, 0, 1920, 1080)],
            vec![
                screen("B1", 0, 0, 2560, 1440),
                screen("B2", 2560, 0, 1920, 1080),
            ],
        )
        .await;

    let a_key = format!("{}/A1", fleet.a.node_id());
    let b_key = format!("{}/B1", fleet.b.node_id());

    // Each waits until its own plane holds all three screens: two rounds of
    // `peers.send`, in whichever order they happen.
    let has_all = |status: &serde_json::Value| {
        let spots = status["plane"]["spots"].as_array();
        spots.is_some_and(|spots| spots.len() == 3)
    };
    let from_a = fleet.a.until("A to hold all three screens", has_all).await;
    let from_b = fleet.b.until("B to hold all three screens", has_all).await;

    assert_eq!(
        from_a["plane"]["id"], from_b["plane"]["id"],
        "the two devices must compute the SAME plane id from the same records:\nA: {from_a:#}\nB: {from_b:#}"
    );

    // And the same rectangles, which is what the id is a hash of. A device's own
    // arrangement is imported as a block rather than re-invented, so B's two
    // screens keep their relative places.
    for status in [&from_a, &from_b] {
        let spots = status["plane"]["spots"].as_array().expect("spots");
        let spot = |key: &str| {
            spots
                .iter()
                .find(|s| s["monitor"] == json!(key))
                .unwrap_or_else(|| panic!("{key} is on the plane: {status:#}"))
        };
        assert_eq!(spot(&a_key)["w"], json!(1920));
        assert_eq!(spot(&a_key)["h"], json!(1080));
        assert_eq!(spot(&a_key)["present"], json!(true));
        assert_eq!(spot(&b_key)["w"], json!(2560));
        // B's second screen sits to the right of its first, exactly as B's own
        // desktop has it.
        let b2 = spot(&format!("{}/B2", fleet.b.node_id()));
        assert_eq!(
            b2["x"].as_i64().expect("x") - spot(&b_key)["x"].as_i64().expect("x"),
            2560,
            "B's own arrangement is imported, not re-invented"
        );
    }
}

/// The arrangement a human drags reaches the other device, and both ends end up
/// on the same plane again. This is the one gesture that writes the plane, and it
/// is last-writer-wins on the whole thing.
#[tokio::test(flavor = "multi_thread")]
async fn an_arrangement_a_human_drags_reaches_the_other_computer() {
    let fleet = Fleet::start().await;
    fleet
        .screens(
            vec![screen("A1", 0, 0, 1920, 1080)],
            vec![screen("B1", 0, 0, 1920, 1080)],
        )
        .await;
    let a_key = format!("{}/A1", fleet.a.node_id());
    let b_key = format!("{}/B1", fleet.b.node_id());
    fleet
        .a
        .until("A to hold both screens", |s| {
            s["plane"]["spots"].as_array().is_some_and(|v| v.len() == 2)
        })
        .await;

    // A human on A puts B's screen to the LEFT of its own.
    fleet
        .a
        .ui
        .request(
            "input.place",
            json!({ "spots": [
                { "monitor": a_key, "x": 0, "y": 0 },
                { "monitor": b_key, "x": -1920, "y": 0 },
            ] }),
        )
        .await
        .expect("input.place");

    let placed = |status: &serde_json::Value| {
        status["plane"]["spots"]
            .as_array()
            .is_some_and(|spots| spots.iter().any(|s| s["x"] == json!(-1920)))
    };
    let from_a = fleet.a.until("A to hold the arrangement", placed).await;
    let from_b = fleet.b.until("the arrangement to reach B", placed).await;
    assert_eq!(
        from_a["plane"]["id"], from_b["plane"]["id"],
        "both ends agree again after the drag"
    );
    assert_eq!(
        from_a["plane"]["by"],
        json!(fleet.a.device_id()),
        "the plane says who arranged it"
    );
    assert_eq!(
        from_b["plane"]["by"],
        json!(fleet.a.device_id()),
        "and says the same thing on the other machine"
    );

    // A spot naming a monitor no record claims is KEPT, which is the ghost rule
    // reaching all the way out to the gesture: the snapshot an interface drags
    // carries spots for screens that are away, marked `present: false`, and it
    // sends the whole set back. Refusing them would make one unplugged screen
    // refuse every future drag on that plane, which is the opposite of "undocking
    // must not lose the arrangement".
    fleet
        .a
        .ui
        .request(
            "input.place",
            json!({ "spots": [
                { "monitor": a_key, "x": 0, "y": 0 },
                { "monitor": b_key, "x": -1920, "y": 0 },
                { "monitor": format!("{}/AWAY", fleet.a.node_id()), "x": -3840, "y": 0 },
            ] }),
        )
        .await
        .expect("an arrangement may place a screen that is not here right now");

    // A key that is not a spot key is still a malformed request, and it is a
    // genuine protocol error rather than an application state.
    let err = fleet
        .a
        .ui
        .request(
            "input.place",
            json!({ "spots": [{ "monitor": "not-a-spot-key", "x": 0, "y": 0 }] }),
        )
        .await
        .expect_err("a spot key that is not one is refused");
    assert!(
        matches!(&err, onedevice_ipc_client::RequestError::Rpc(e) if e.code == -32602),
        "the shape is a protocol error, got {err:?}"
    );
}

/// A screen that goes away keeps its place, and says so. Undocking must not lose
/// the arrangement, and the edges facing an absent screen become walls so the
/// pointer is never swallowed by a screen that is not there.
#[tokio::test(flavor = "multi_thread")]
async fn a_screen_that_is_unplugged_keeps_its_place_and_says_so() {
    let fleet = Fleet::start().await;
    // ARRANGED, because "its place is kept" needs a place: an unplugged screen
    // nobody ever dragged has no spot to keep, and simply leaves the plane. That
    // is a real limit of the design and it is section 6's wording, not a bug.
    fleet
        .arranged(
            vec![screen("A1", 0, 0, 1920, 1080)],
            vec![
                screen("B1", 0, 0, 1920, 1080),
                screen("B2", 1920, 0, 1280, 800),
            ],
        )
        .await;
    let gone = format!("{}/B2", fleet.b.node_id());
    fleet
        .a
        .until("A to hold all three screens", |s| {
            s["plane"]["spots"].as_array().is_some_and(|v| v.len() == 3)
        })
        .await;
    let before = fleet.a.status().await;
    let spot_of = |status: &serde_json::Value, key: &str| {
        status["plane"]["spots"]
            .as_array()
            .expect("spots")
            .iter()
            .find(|s| s["monitor"] == json!(key))
            .cloned()
    };
    let placed_at = spot_of(&before, &gone).expect("B2 is on the plane");

    // B unplugs its second screen.
    fleet
        .b
        .set_monitors(vec![screen("B1", 0, 0, 1920, 1080)])
        .await;

    let after = fleet
        .a
        .until("A to hear that B2 is away", |status| {
            spot_of(status, &gone).is_some_and(|s| s["present"] == json!(false))
        })
        .await;
    let ghost = spot_of(&after, &gone).expect("its place is kept");
    assert_eq!(
        (ghost["x"].clone(), ghost["y"].clone()),
        (placed_at["x"].clone(), placed_at["y"].clone()),
        "an absent screen keeps exactly the place it had"
    );
    assert_eq!(ghost["present"], json!(false));
}

/// The plane is bounded by the account, not by what a peer says: nothing a peer
/// sends can grow it past the document's own limits, and a plane that came back
/// from disk is the same plane.
#[tokio::test(flavor = "multi_thread")]
async fn the_plane_survives_a_restart_unchanged() {
    let mut fleet = Fleet::start().await;
    fleet
        .screens(
            vec![screen("A1", 0, 0, 1920, 1080)],
            vec![screen("B1", 0, 0, 1920, 1080)],
        )
        .await;
    let a_key = format!("{}/A1", fleet.a.node_id());
    let b_key = format!("{}/B1", fleet.b.node_id());
    fleet
        .a
        .ui
        .request(
            "input.place",
            json!({ "spots": [
                { "monitor": a_key, "x": 0, "y": 0 },
                { "monitor": b_key, "x": 1920, "y": 240 },
            ] }),
        )
        .await
        .expect("input.place");
    let before = fleet
        .a
        .until("the arrangement to be recorded", |s| {
            s["plane"]["spots"]
                .as_array()
                .is_some_and(|spots| spots.iter().any(|s| s["y"] == json!(240)))
        })
        .await;

    fleet.a.stop_engine().await;
    let restarted = Device::start(fleet.a.core).await;
    // The restart gets a FRESH fake backend, so it comes up claiming the fake's
    // default screen: a real machine keeps its monitors across a restart, and this
    // one has to be told them again, or it would be a different machine and the
    // plane would rightly say so.
    restarted
        .set_monitors(vec![screen("A1", 0, 0, 1920, 1080)])
        .await;
    let after = restarted
        .until("the plane to come back from disk", |s| {
            s["plane"]["spots"].as_array().is_some_and(|v| v.len() == 2)
        })
        .await;
    assert_eq!(
        after["plane"]["id"], before["plane"]["id"],
        "the plane that came back from disk is the same plane"
    );
    // The arrangement is the half that could only have come off the disk: nobody
    // re-dragged anything, and the seq and the author survived with it.
    assert_eq!(after["plane"]["by"], before["plane"]["by"]);
}
