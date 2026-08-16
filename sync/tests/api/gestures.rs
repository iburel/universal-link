// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The frozen vocabulary end to end: an interface calling `sync.*` through
//! the Core's routed facade, against the real engine over real IPC. What
//! the gestures do to the engine's own state is unit-tested at the engine
//! layer; here we pin the CONTRACT - the shapes, the refusals, the
//! authoritative snapshot, and the notifications that echo it.

use serde_json::json;

use crate::support::{Engine, app_code, core_in_account, ready, ui, wait_notification};

/// Creating a set answers with its id, and the snapshot then carries a card
/// the interface can render whole.
#[tokio::test(flavor = "multi_thread")]
async fn create_answers_a_set_id_and_the_snapshot_carries_its_card() {
    let core = core_in_account().await;
    let engine = Engine::start(&core);
    let mut ui = ui(&core).await;
    ready(&ui).await;

    let root = tempfile::tempdir().expect("root");
    std::fs::write(root.path().join("a.txt"), "alpha").expect("write");
    let reply = ui
        .client
        .request(
            "sync.create",
            json!({ "path": root.path().to_string_lossy(), "device_ids": [] }),
        )
        .await
        .expect("create");
    let set_id = reply["set_id"].as_str().expect("set_id").to_string();
    assert_eq!(set_id.len(), 22, "an unguessable set id");

    // The card arrives on the topic AND in the snapshot: the notification
    // echoes the authoritative state, it does not lead it.
    let updated = wait_notification(&mut ui, "sync.updated").await;
    assert_eq!(updated["set"]["set_id"], json!(set_id));

    let status = ui
        .client
        .request("sync.status", json!({}))
        .await
        .expect("status");
    let card = &status["sets"][0];
    assert_eq!(card["set_id"], json!(set_id));
    assert_eq!(card["kind"], json!("dir"));
    assert_eq!(
        card["name"],
        json!(root.path().file_name().unwrap().to_string_lossy())
    );
    assert_eq!(card["path"], json!(root.path().to_string_lossy()));
    // Alone in the set: nothing to be in order with.
    assert_eq!(card["state"], json!("waiting"));
    assert_eq!(card["problem"], json!(null));
    assert!(card["devices"].is_array());
    assert!(card["conflicts"].is_array());
    assert!(card["ignored"].is_array());
    assert!(card["blocked"].is_array());
    assert_eq!(status["invitations"], json!([]));

    assert_eq!(engine.stop().await, onedevice_sync::Outcome::StdinClosed);
}

/// Pause, resume and leave travel through the facade and land on the card;
/// leaving takes the set off the snapshot and says so.
#[tokio::test(flavor = "multi_thread")]
async fn the_lifecycle_gestures_move_the_card_and_leaving_removes_it() {
    let core = core_in_account().await;
    let engine = Engine::start(&core);
    let mut ui = ui(&core).await;
    ready(&ui).await;

    let root = tempfile::tempdir().expect("root");
    let set_id = ui
        .client
        .request(
            "sync.create",
            json!({ "path": root.path().to_string_lossy() }),
        )
        .await
        .expect("create")["set_id"]
        .as_str()
        .expect("set_id")
        .to_string();

    ui.client
        .request("sync.pause", json!({ "set_id": set_id }))
        .await
        .expect("pause");
    let status = ui
        .client
        .request("sync.status", json!({}))
        .await
        .expect("status");
    assert_eq!(status["sets"][0]["state"], json!("paused"));

    ui.client
        .request("sync.resume", json!({ "set_id": set_id }))
        .await
        .expect("resume");
    let status = ui
        .client
        .request("sync.status", json!({}))
        .await
        .expect("status");
    assert_ne!(status["sets"][0]["state"], json!("paused"));

    ui.client
        .request("sync.leave", json!({ "set_id": set_id }))
        .await
        .expect("leave");
    let removed = wait_notification(&mut ui, "sync.removed").await;
    assert_eq!(removed["set_id"], json!(set_id));
    let status = ui
        .client
        .request("sync.status", json!({}))
        .await
        .expect("status");
    assert_eq!(status["sets"], json!([]), "the set left this device");
    // The local files stay where they are (the letter, and the interface
    // says so).
    assert!(root.path().exists());

    assert_eq!(engine.stop().await, onedevice_sync::Outcome::StdinClosed);
}

/// Every refusal the vocabulary promises, with its own code.
#[tokio::test(flavor = "multi_thread")]
async fn the_refusals_carry_their_documented_codes() {
    let core = core_in_account().await;
    let engine = Engine::start(&core);
    let ui = ui(&core).await;
    ready(&ui).await;

    // A set nobody holds.
    assert_eq!(
        app_code(
            &ui,
            "sync.pause",
            json!({ "set_id": "BBBBBBBBBBBBBBBBBBBBBB" })
        )
        .await,
        "SYNC_UNKNOWN_SET"
    );
    // Shapes: the JSON-RPC -32602, not an application state.
    assert_eq!(app_code(&ui, "sync.pause", json!({})).await, "code -32602");
    assert_eq!(
        app_code(&ui, "sync.create", json!({ "path": "relative/path" })).await,
        "code -32602"
    );
    // A device the directory does not name.
    let root = tempfile::tempdir().expect("root");
    assert_eq!(
        app_code(
            &ui,
            "sync.create",
            json!({ "path": root.path().to_string_lossy(), "device_ids": ["d_nobody"] }),
        )
        .await,
        "SYNC_DEVICE_INELIGIBLE"
    );
    // ...and the refused gesture left no half-made set behind.
    let status = ui
        .client
        .request("sync.status", json!({}))
        .await
        .expect("status");
    assert_eq!(status["sets"], json!([]));

    // One file, one set.
    let set_id = ui
        .client
        .request(
            "sync.create",
            json!({ "path": root.path().to_string_lossy() }),
        )
        .await
        .expect("create")["set_id"]
        .as_str()
        .expect("set_id")
        .to_string();
    assert_eq!(
        app_code(
            &ui,
            "sync.create",
            json!({ "path": root.path().to_string_lossy() }),
        )
        .await,
        "SYNC_ROOT_OVERLAP"
    );
    let inside = root.path().join("nested");
    std::fs::create_dir(&inside).expect("mkdir");
    assert_eq!(
        app_code(
            &ui,
            "sync.create",
            json!({ "path": inside.to_string_lossy() }),
        )
        .await,
        "SYNC_ROOT_OVERLAP"
    );

    // Accepting what nobody invited us to.
    let other = tempfile::tempdir().expect("other");
    assert_eq!(
        app_code(
            &ui,
            "sync.accept",
            json!({ "set_id": set_id, "path": other.path().join("fresh").to_string_lossy() }),
        )
        .await,
        "SYNC_NOT_INVITED"
    );
    // Declining what nobody invited us to.
    assert_eq!(
        app_code(&ui, "sync.decline", json!({ "set_id": set_id })).await,
        "SYNC_NOT_INVITED"
    );
    // Resolving a conflict that does not exist.
    assert_eq!(
        app_code(
            &ui,
            "sync.resolve",
            json!({ "set_id": set_id, "path": "a.txt", "keep": "all" }),
        )
        .await,
        "SYNC_NO_CONFLICT"
    );

    assert_eq!(engine.stop().await, onedevice_sync::Outcome::StdinClosed);
}

/// The exclusions the scan refuses ride the card, with their reasons: the
/// interface can show them without asking anything else.
#[tokio::test(flavor = "multi_thread")]
async fn the_card_carries_the_ignored_names_and_their_reasons() {
    let core = core_in_account().await;
    let engine = Engine::start(&core);
    let mut ui = ui(&core).await;
    ready(&ui).await;

    let root = tempfile::tempdir().expect("root");
    std::fs::write(root.path().join("fine.txt"), "x").expect("write");
    std::fs::write(root.path().join("CON"), "reserved").expect("write");
    ui.client
        .request(
            "sync.create",
            json!({ "path": root.path().to_string_lossy() }),
        )
        .await
        .expect("create");
    // The first card may precede the scan; the ignored list arrives with it.
    let ignored = loop {
        let updated = wait_notification(&mut ui, "sync.updated").await;
        let list = updated["set"]["ignored"].clone();
        if list.as_array().is_some_and(|l| !l.is_empty()) {
            break list;
        }
    };
    assert_eq!(ignored[0]["path"], json!("CON"));
    assert_eq!(ignored[0]["reason"], json!("reserved device name"));

    assert_eq!(engine.stop().await, onedevice_sync::Outcome::StdinClosed);
}
