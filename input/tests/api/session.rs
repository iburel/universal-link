// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! A session between two real devices: who may start one, who may not, and what
//! happens when two machines want the same keyboard
//! (doc/input-sharing.md, section 4).
//!
//! The state machine's own arithmetic (the dwell, the double tap, the coalescing,
//! the flow counter) is unit-tested in `src/session.rs` with time passed in
//! explicitly. What this file proves is what only two real Cores can: that the
//! grant is the far side's authority and is learned by TRYING, that the exclusion
//! really holds when two engines want the same thing, and that a keystroke a
//! human made on one machine is really typed on the other.

use onedevice_input::backend::BackendEvent;
use serde_json::json;

use crate::support::*;

/// The grant is the far machine's authority, and the driving side learns by
/// trying. Nothing before the attempt hints at it, which is the doctrine: a grant
/// can be withdrawn between a hint and its use.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_grant_the_far_side_refuses_and_says_so() {
    let fleet = Fleet::start().await;
    fleet
        .screens(
            vec![screen("A1", 0, 0, 1920, 1080)],
            vec![screen("B1", 0, 0, 1920, 1080)],
        )
        .await;
    fleet.a.drive(&fleet.b, true).await;

    // A's own snapshot cannot know whether B would accept: it says only what A
    // itself has decided.
    let row = fleet
        .a
        .until_peer(fleet.b.device_id(), "A to be willing to drive B", |row| {
            row["drive"] == json!(true)
        })
        .await;
    assert_eq!(
        row["allowed"],
        json!(false),
        "A's row for B is A's own word, never B's"
    );

    // The first attempt is ACCEPTED locally: there is nothing here that could
    // know B's answer, and inventing one would be the cached grant the doctrine
    // forbids. What comes back is a refusal on the wire.
    fleet
        .a
        .ui
        .request("input.take", json!({ "device_id": fleet.b.device_id() }))
        .await
        .expect("asking is always allowed: it is the answer that refuses");

    // And the answer is a sentence the interface can say, recorded on the row
    // rather than lost, with no session anywhere.
    fleet
        .a
        .until_peer(fleet.b.device_id(), "the refusal to be on the row", |row| {
            row["problem"] == json!("not_allowed") && row["state"] != json!("driving")
        })
        .await;
    assert_eq!(fleet.a.status().await["session"], json!(null));

    // Asked again, the gesture is accepted again: this machine still knows
    // nothing about B's grant, and answering from the refusal it just heard would
    // be a cached grant in all but name, wrong the instant B flips the switch.
    fleet
        .a
        .ui
        .request("input.take", json!({ "device_id": fleet.b.device_id() }))
        .await
        .expect("asking is always allowed");

    // Which is exactly what happens next: B flips the switch and the same call
    // works, with no dead window in between.
    fleet.b.allow(&fleet.a, true).await;
    fleet
        .a
        .ui
        .request("input.take", json!({ "device_id": fleet.b.device_id() }))
        .await
        .expect("input.take once the far side allows it");
    fleet
        .a
        .until_peer(fleet.b.device_id(), "A to be driving B", |row| {
            row["state"] == json!("driving")
        })
        .await;
}

/// A keystroke really crosses: a key A's backend reports is a key B's backend
/// really presses, resolved on B's own layout. The whole point of the feature, at
/// its smallest.
#[tokio::test(flavor = "multi_thread")]
async fn a_keystroke_made_on_one_machine_is_typed_on_the_other() {
    use onedevice_input::backend::{Action, KeyEvent, PlatformKey};
    use onedevice_input::keys::mods;

    let fleet = Fleet::start().await;
    // B can produce an at sign, and on B it takes AltGr and the key at code 48.
    // That is the epic's own example, and here it crosses two real Cores.
    //
    // Taught BEFORE the session starts, which is load bearing and cost an
    // afternoon: the engine asks its backend for all five modifier keys once, at
    // session start, and the resolver caches a NEGATIVE answer deliberately (a
    // symbol this layout cannot produce must not cost a round trip per
    // keystroke). Teaching AltGr afterwards therefore changes nothing, the stroke
    // is abandoned whole rather than injected with half its modifiers, which is
    // right, and it degrades to Unicode. A real backend whose keymap changes
    // says so with a `LayoutChanged`, which is also how a test can teach late.
    let altgr = onedevice_input::keys::mod_usage(mods::ALTGR).expect("altgr has a usage");
    fleet.b.backend.teach_usage(altgr, 165);
    fleet.b.backend.teach_symbol("@", 48, mods::ALTGR);
    fleet.driving().await;
    fleet.b.backend.forget();

    // A's human types the QWERTY at sign: the position of the 2 key, with Shift.
    fleet
        .a
        .backend
        .emit(BackendEvent::Key(KeyEvent {
            usage: onedevice_input::keys::usage(onedevice_input::keys::PAGE_KEYBOARD, 0x1F),
            key: None,
            sym: Some("@".into()),
            mods: mods::SHIFT,
            down: true,
            lock: false,
        }))
        .await;

    let target = fleet.b.backend.clone();
    wait_until("B to type the at sign", || {
        target.calls().actions.iter().any(|a| {
            matches!(
                a,
                Action::Key {
                    code: PlatformKey { code: 48, .. },
                    down: true
                }
            )
        })
    })
    .await;
    let calls = target.calls();
    // And it took AltGr to do it, not the position A sent: symbol first is what
    // makes the epic's example work at all.
    assert!(
        calls.actions.iter().any(|a| matches!(
            a,
            Action::Key {
                code: PlatformKey { code: 165, .. },
                down: true
            }
        )),
        "the at sign was produced with AltGr on this layout: {calls:?}"
    );
}

/// One source at a time, and no preemption. A second machine asking for a
/// keyboard that is held is told who holds it, and nothing is taken from anybody.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_source_is_told_who_holds_the_keyboard_and_takes_nothing() {
    let fleet = Fleet::start().await;
    fleet.driving().await;

    // B is being driven by A. B now asks to drive A: refused, because a machine
    // being driven cannot also drive (which is what makes echo suppression a
    // non-problem in v1).
    fleet.a.allow(&fleet.b, true).await;
    fleet.b.drive(&fleet.a, true).await;
    assert_eq!(
        fleet
            .b
            .ui
            .refusal("input.take", json!({ "device_id": fleet.a.device_id() }))
            .await,
        "INPUT_BUSY",
        "a machine being driven does not also drive"
    );

    // And A's session is untouched: nothing was preempted.
    let row = fleet
        .a
        .until_peer(fleet.b.device_id(), "A to still be driving B", |row| {
            row["state"] == json!("driving")
        })
        .await;
    assert_eq!(row["state"], json!("driving"));
    let target = fleet.b.backend.clone();
    assert!(
        !target.keys_down().is_empty(),
        "the modifier the live session is holding was not released by the refusal"
    );
}

/// A machine that cannot type says so, and the machine that wanted to drive it
/// learns it as that device's `problem`, which is what lets the interface explain
/// instead of showing a refusal per keystroke.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_that_cannot_type_refuses_the_session_and_says_why() {
    let fleet = Fleet::start().await;
    fleet
        .screens(
            vec![screen("A1", 0, 0, 1920, 1080)],
            vec![screen("B1", 0, 0, 1920, 1080)],
        )
        .await;
    fleet.b.allow(&fleet.a, true).await;
    fleet.a.drive(&fleet.b, true).await;

    // B's OS grant goes away: nothing there can type any more.
    fleet.b.backend.refused();
    fleet
        .b
        .backend
        .emit(BackendEvent::CaptureLost(
            onedevice_input::backend::CaptureLoss::Permission,
        ))
        .await;
    fleet
        .b
        .until("B to report its missing permission", |status| {
            status["here"]["problem"] == json!("no_permission")
                && status["here"]["can_be_driven"] == json!(false)
        })
        .await;

    // A takes the keyboard there anyway, because nothing here knows B's state:
    // the far side's own word is what refuses, and it lands on the row.
    fleet
        .a
        .ui
        .request("input.take", json!({ "device_id": fleet.b.device_id() }))
        .await
        .expect("asking is always allowed");
    fleet
        .a
        .until_peer(fleet.b.device_id(), "B's refusal to reach A", |row| {
            row["problem"] == json!("no_backend")
        })
        .await;
    assert_eq!(
        fleet.a.status().await["session"],
        json!(null),
        "and no session was left behind"
    );
}

/// Withdrawing the grant while a session runs ends it: the authority is the
/// target's, at every moment and not only at the start.
#[tokio::test(flavor = "multi_thread")]
async fn withdrawing_the_grant_mid_session_ends_it() {
    let fleet = Fleet::start().await;
    fleet.driving().await;

    fleet.b.allow(&fleet.a, false).await;

    let target = fleet.b.backend.clone();
    wait_until("B to release what it was holding", || {
        target.keys_down().is_empty()
    })
    .await;
    fleet
        .b
        .until("B to have no session", |status| {
            status["session"] == json!(null)
        })
        .await;
    let source = fleet.a.backend.clone();
    wait_until("A to bring the keyboard home", || {
        source.calls().confine.last() == Some(&None)
    })
    .await;
}

/// Locking the pointer to this screen refuses a session outright: it is the one
/// guard that is not about an edge, and a game or a virtual machine relies on it
/// holding absolutely.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_machine_neither_drives_nor_is_driven() {
    let fleet = Fleet::start().await;
    fleet
        .screens(
            vec![screen("A1", 0, 0, 1920, 1080)],
            vec![screen("B1", 0, 0, 1920, 1080)],
        )
        .await;
    fleet.b.allow(&fleet.a, true).await;
    fleet.a.drive(&fleet.b, true).await;

    fleet
        .b
        .ui
        .request("input.lock", json!({ "locked": true }))
        .await
        .expect("input.lock");
    fleet
        .b
        .until("B to be locked", |status| status["lock"] == json!(true))
        .await;

    // A locked machine does not take anybody else's keyboard: the gesture is
    // refused HERE, synchronously, because the lock is this machine's own word
    // about its own screen and needs no round trip. `INPUT_LOCKED` and not
    // `INPUT_BUSY`, deliberately: nobody is holding it, and the remedy is a
    // switch rather than waiting.
    fleet.a.allow(&fleet.b, true).await;
    fleet.b.drive(&fleet.a, true).await;
    assert_eq!(
        fleet
            .b
            .ui
            .refusal("input.take", json!({ "device_id": fleet.a.device_id() }))
            .await,
        "INPUT_LOCKED"
    );

    // And B will not be driven either, which A learns the only honest way: by
    // asking, and reading B's word off the row.
    fleet
        .a
        .ui
        .request("input.take", json!({ "device_id": fleet.b.device_id() }))
        .await
        .expect("asking is always allowed");
    fleet
        .a
        .until_peer(fleet.b.device_id(), "B's lock to reach A", |row| {
            row["problem"] == json!("locked")
        })
        .await;
    assert_eq!(fleet.a.status().await["session"], json!(null));
}
