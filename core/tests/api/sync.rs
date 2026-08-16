// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The routed `sync.*` facade (doc/core-api.md, "sync.*"; ticket #83).
//!
//! Methods of the `sync.*` namespace are forwarded by the Core to the
//! connected exclusive `sync-backend` (the `clipboard.get_data` pattern, in
//! the other direction) and the reply relays verbatim, errors included: the
//! vocabulary's semantics live in the component, the Core checks scopes and
//! caches nothing. `sync.status` (the topic's snapshot method) rides
//! `sync.read`; every other gesture rides `sync.manage`. No backend, a dead
//! one, or a silent one: `COMPONENT_ABSENT`. The backend publishes the `sync`
//! topic through `sync.emit`, gated on its role AND `sync.serve` like
//! announcing is for the clipboard.

use serde_json::json;

use crate::support::*;

/// The official engine's shape: the exclusive role, serving scope.
async fn backend(core: &TestCore) -> TestComponent {
    spawn_component(core, "sync-engine", "sync-backend", &["sync.serve"]).await
}

/// An interface holding both facade scopes.
async fn interface(core: &TestCore) -> TestComponent {
    spawn_component(core, "ui", "custom", &["sync.read", "sync.manage"]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_facade_forwards_and_relays_the_reply() {
    let core = TestCore::start().await;
    let mut engine = backend(&core).await;
    let mut ui = interface(&core).await;

    let ask = ui.request("sync.pause", json!({ "set": "s1" }));
    let serve = async {
        let (id, params) = engine.expect_request("sync.pause").await;
        assert_eq!(params, json!({ "set": "s1" }));
        engine.respond(id, json!({ "paused": true })).await;
    };
    let (reply, ()) = tokio::join!(ask, serve);
    assert_eq!(reply.expect("forwarded"), json!({ "paused": true }));
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_errors_relay_verbatim() {
    let core = TestCore::start().await;
    let mut engine = backend(&core).await;
    let mut ui = interface(&core).await;

    let ask = ui.request("sync.resolve", json!({ "conflict": "c9" }));
    let serve = async {
        let (id, _) = engine.expect_request("sync.resolve").await;
        engine.respond_error(id, "CONFLICT_UNKNOWN").await;
    };
    let (reply, ()) = tokio::join!(ask, serve);
    // The engine's own vocabulary crosses the Core untouched.
    assert_eq!(reply.unwrap_err().app_code(), "CONFLICT_UNKNOWN");
}

#[tokio::test]
async fn component_absent_without_a_serving_backend() {
    let core = TestCore::start().await;
    let mut ui = interface(&core).await;

    // Nobody holds the role.
    let err = ui.request("sync.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "COMPONENT_ABSENT");

    // The role without the serving scope is not a backend either.
    let _mute = spawn_component(&core, "mute", "sync-backend", &["session.read"]).await;
    let err = ui.request("sync.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "COMPONENT_ABSENT");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backend_that_dies_mid_flight_reads_as_absent() {
    let core = TestCore::start().await;
    let mut engine = backend(&core).await;
    let mut ui = interface(&core).await;

    let ask = ui.request("sync.leave", json!({ "set": "s1" }));
    let serve = async {
        // The backend reads the forward, then tears down without answering.
        let _ = engine.expect_request("sync.leave").await;
        drop(engine);
    };
    let (reply, ()) = tokio::join!(ask, serve);
    assert_eq!(reply.unwrap_err().app_code(), "COMPONENT_ABSENT");
}

#[tokio::test]
async fn the_read_manage_split_guards_the_facade() {
    let core = TestCore::start().await;
    let _engine = backend(&core).await;

    // A reader may snapshot, not gesture.
    let mut reader = spawn_component(&core, "reader", "custom", &["sync.read"]).await;
    let err = reader
        .request("sync.pause", json!({ "set": "s1" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");

    // A manager may gesture; the snapshot is the reader's privilege - the
    // split is frozen, an interface that wants both holds both.
    let mut manager = spawn_component(&core, "manager", "custom", &["sync.manage"]).await;
    let err = manager.request("sync.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");

    // Phase before everything: an unenrolled connection learns nothing.
    let mut fresh = core.connect().await;
    let err = fresh.request("sync.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "NOT_ENROLLED");
}

#[tokio::test]
async fn the_sync_backend_role_is_exclusive_and_coexists_with_the_clipboard_one() {
    let core = TestCore::start().await;
    let _engine = backend(&core).await;

    // A second engine: refused, and the refused token is not consumed.
    let token = core.mint("sync-backend", &["sync.serve"]);
    let mut second = core.connect().await;
    let err = second
        .hello("late-engine", "sync-backend", &["sync.serve"], Some(&token))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "ROLE_CONFLICT");

    // The two exclusive roles are two SLOTS, not one.
    let _clip = spawn_component(
        &core,
        "clipboard",
        "clipboard-backend",
        &["clipboard.read", "clipboard.write"],
    )
    .await;
}

#[tokio::test]
async fn sync_emit_publishes_the_topic_through_the_core() {
    let core = TestCore::start().await;
    let mut engine = backend(&core).await;

    // A subscriber with the topic's scope hears the engine's word verbatim.
    let mut ui = interface(&core).await;
    ui.request("events.subscribe", json!({ "topics": ["sync"] }))
        .await
        .expect("subscribe the sync topic");
    // A reader NOT subscribed hears nothing (subscription-based, not a duty
    // push).
    let mut bystander = spawn_component(&core, "bystander", "custom", &["sync.read"]).await;

    engine
        .request(
            "sync.emit",
            json!({ "method": "sync.conflict", "params": { "set": "s1", "files": 3 } }),
        )
        .await
        .expect("sync.emit");
    let params = ui.wait_notification("sync.conflict").await;
    assert_eq!(params, json!({ "set": "s1", "files": 3 }));
    bystander.assert_silent().await;

    // The topic is gated on `sync.read` at subscription.
    let mut blind = spawn_component(&core, "blind", "custom", &["sync.manage"]).await;
    let err = blind
        .request("events.subscribe", json!({ "topics": ["sync"] }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn sync_emit_is_the_backends_privilege_and_checks_its_shape() {
    let core = TestCore::start().await;
    let mut engine = backend(&core).await;

    // An interface holding every facade scope still cannot publish the topic.
    let mut ui = interface(&core).await;
    let err = ui
        .request("sync.emit", json!({ "method": "sync.card", "params": {} }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");

    // A notification outside the namespace, a reserved name, or params that
    // are not an object: refused, nothing published.
    for bad in [
        json!({ "method": "devices.updated", "params": {} }),
        json!({ "method": "sync.emit", "params": {} }),
        json!({ "method": "sync.card", "params": "not-an-object" }),
        json!({ "method": "sync.card" }),
    ] {
        let err = engine.request("sync.emit", bad).await.unwrap_err();
        assert_eq!(err.code, -32602);
    }
}
