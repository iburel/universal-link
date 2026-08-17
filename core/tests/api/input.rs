// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The routed `input.*` facade (doc/core-api.md, "input.*"; ticket #126).
//!
//! The sync facade's doctrine, applied to the keyboard and the mouse: every
//! method of the `input.*` namespace except `input.emit` is forwarded by the
//! Core to the connected exclusive `input-backend`, and the reply relays
//! verbatim, errors included - the vocabulary's semantics live in the engine
//! (doc/input-sharing.md), the Core checks scopes and shapes and caches
//! nothing. That last point is the design and not modesty: who may drive this
//! computer is the engine's authority, and a copy kept in the Core would be a
//! second answer to a question that has exactly one.
//!
//! `input.status` (the topic's snapshot method) rides `input.read`; every other
//! gesture rides `input.manage`. No engine, one that dies mid-flight, or one
//! that stays silent past the proxy budget: `COMPONENT_ABSENT`. The engine
//! publishes the `input` topic through `input.emit`, gated on its role AND
//! `input.serve` exactly like `sync.emit` is.

use serde_json::json;

use crate::support::*;

/// The official engine's shape: the exclusive role, serving scope.
async fn engine(core: &TestCore) -> TestComponent {
    spawn_component(core, "input-engine", "input-backend", &["input.serve"]).await
}

/// An interface holding both facade scopes.
async fn interface(core: &TestCore) -> TestComponent {
    spawn_component(core, "ui", "custom", &["input.read", "input.manage"]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_facade_forwards_a_gesture_and_relays_the_reply() {
    let core = TestCore::start().await;
    let mut eng = engine(&core).await;
    let mut ui = interface(&core).await;

    let ask = ui.request(
        "input.take",
        json!({ "device_id": "d_far", "mode": "screen" }),
    );
    let serve = async {
        // The params cross untouched: the Core adds nothing and drops nothing.
        let (id, params) = eng.expect_request("input.take").await;
        assert_eq!(params, json!({ "device_id": "d_far", "mode": "screen" }));
        eng.respond(id, json!({ "driving": "d_far" })).await;
    };
    let (reply, ()) = tokio::join!(ask, serve);
    assert_eq!(reply.expect("forwarded"), json!({ "driving": "d_far" }));
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_errors_relay_verbatim() {
    let core = TestCore::start().await;
    let mut eng = engine(&core).await;
    let mut ui = interface(&core).await;

    let ask = ui.request(
        "input.allow",
        json!({ "device_id": "d_far", "allowed": true }),
    );
    let serve = async {
        let (id, _) = eng.expect_request("input.allow").await;
        eng.respond_error(id, "INPUT_DEVICE_UNKNOWN").await;
    };
    let (reply, ()) = tokio::join!(ask, serve);
    // The engine's own vocabulary crosses the Core untouched: the JSON-RPC code
    // AND the application code, or an interface could not tell an engine's
    // refusal from the Core's.
    let err = reply.unwrap_err();
    assert_eq!(err.code, -32000);
    assert_eq!(err.app_code(), "INPUT_DEVICE_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_read_manage_split_guards_the_facade() {
    let core = TestCore::start().await;
    let mut eng = engine(&core).await;

    // A reader may snapshot: the call is forwarded and the engine's answer
    // comes back to it.
    let mut reader = spawn_component(&core, "reader", "custom", &["input.read"]).await;
    let ask = reader.request("input.status", json!({}));
    let serve = async {
        let (id, _) = eng.expect_request("input.status").await;
        eng.respond(id, json!({ "spots": [], "devices": [] })).await;
    };
    let (reply, ()) = tokio::join!(ask, serve);
    assert_eq!(
        reply.expect("snapshot"),
        json!({ "spots": [], "devices": [] })
    );

    // Not gesture, though: reading where the screens sit and handing another
    // computer the right to type here are two different rights.
    let err = reader
        .request("input.take", json!({ "device_id": "d_far" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");

    // A manager may gesture; the snapshot is the reader's privilege - the
    // split is per scope, an interface that wants both holds both.
    let mut manager = spawn_component(&core, "manager", "custom", &["input.manage"]).await;
    let err = manager
        .request("input.status", json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");

    // Neither refusal ever reached the engine: the scope check happens before
    // anything is forwarded.
    eng.assert_silent().await;

    // Phase before everything: an unenrolled connection learns nothing, of the
    // facade or of the topic's publication.
    let mut fresh = core.connect().await;
    let err = fresh.request("input.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "NOT_ENROLLED");
    let err = fresh
        .request(
            "input.emit",
            json!({ "method": "input.updated", "params": {} }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "NOT_ENROLLED");
}

#[tokio::test]
async fn component_absent_without_a_serving_engine() {
    let core = TestCore::start().await;
    let mut ui = interface(&core).await;

    // Nobody holds the role.
    let err = ui.request("input.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "COMPONENT_ABSENT");

    // The role without the serving scope is not an engine either - neither to
    // answer the facade, nor to publish the topic.
    let mut mute = spawn_component(&core, "mute", "input-backend", &["session.read"]).await;
    let err = ui.request("input.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "COMPONENT_ABSENT");
    let err = mute
        .request(
            "input.emit",
            json!({ "method": "input.updated", "params": {} }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_that_dies_mid_flight_reads_as_absent() {
    let core = TestCore::start().await;
    let mut eng = engine(&core).await;
    let mut ui = interface(&core).await;

    let ask = ui.request("input.release", json!({}));
    let serve = async {
        // The engine reads the forward, then tears down without answering: the
        // interface hears an honest absence rather than waiting out the budget.
        let _ = eng.expect_request("input.release").await;
        drop(eng);
    };
    let (reply, ()) = tokio::join!(ask, serve);
    assert_eq!(reply.unwrap_err().app_code(), "COMPONENT_ABSENT");
}

#[tokio::test]
async fn the_engine_calling_the_facade_is_answered_absent_at_once() {
    let core = TestCore::start().await;
    // An engine that also holds a facade scope: forwarding to ONESELF would
    // enqueue a request its own blocked dispatch could never answer, so the
    // Core refuses immediately rather than burning the whole forward budget.
    let mut eng = spawn_component(
        &core,
        "input-engine",
        "input-backend",
        &["input.serve", "input.read"],
    )
    .await;
    let err = eng.request("input.status", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "COMPONENT_ABSENT");
}

#[tokio::test]
async fn the_input_backend_role_is_exclusive_and_its_slot_is_its_own() {
    let core = TestCore::start().await;
    let first = engine(&core).await;

    // A second engine: refused, and the refused token is not consumed.
    let token = core.mint("input-backend", &["input.serve"]);
    let mut second = core.connect().await;
    let err = second
        .hello(
            "late-engine",
            "input-backend",
            &["input.serve"],
            Some(&token),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "ROLE_CONFLICT");

    // Every exclusive role is its OWN slot: the clipboard and sync engines are
    // unaffected by the input one holding its.
    let _clip = spawn_component(
        &core,
        "clipboard",
        "clipboard-backend",
        &["clipboard.read", "clipboard.write"],
    )
    .await;
    let _sync = spawn_component(&core, "sync-engine", "sync-backend", &["sync.serve"]).await;

    // The slot frees when its holder disconnects, and the token refused above
    // still works: replacing the official engine is a configuration choice.
    drop(first);
    eventually(
        async || {
            matches!(
                second
                    .hello("late-engine", "input-backend", &["input.serve"], Some(&token))
                    .await,
                Ok(v) if v["status"] == "ok"
            )
        },
        "input-backend role taken over after the holder disconnects",
    )
    .await;
}

#[tokio::test]
async fn input_emit_publishes_the_topic_through_the_core() {
    let core = TestCore::start().await;
    let mut eng = engine(&core).await;

    // A subscriber with the topic's scope hears the engine's word verbatim.
    let mut ui = interface(&core).await;
    ui.request("events.subscribe", json!({ "topics": ["input"] }))
        .await
        .expect("subscribe the input topic");
    // A reader NOT subscribed hears nothing (subscription-based, not a duty
    // push).
    let mut bystander = spawn_component(&core, "bystander", "custom", &["input.read"]).await;

    eng.request(
        "input.emit",
        json!({ "method": "input.updated", "params": { "state": { "driving": "d_far" } } }),
    )
    .await
    .expect("input.emit");
    let params = ui.wait_notification("input.updated").await;
    assert_eq!(params, json!({ "state": { "driving": "d_far" } }));

    // The second notification of the contract, refused crossings, rides the
    // same path. Awaited in the order it was published: `wait_notification`
    // DISCARDS what it skips, so an assertion on an event already thrown away
    // would wait forever.
    eng.request(
        "input.emit",
        json!({ "method": "input.refused", "params": { "device_id": "d_far", "code": "INPUT_NOT_ALLOWED", "count": 3 } }),
    )
    .await
    .expect("input.emit");
    let params = ui.wait_notification("input.refused").await;
    assert_eq!(params["count"], json!(3));
    bystander.assert_silent().await;

    // The topic is gated on `input.read` at subscription: a manager may drive,
    // and still not watch.
    let mut blind = spawn_component(&core, "blind", "custom", &["input.manage"]).await;
    let err = blind
        .request("events.subscribe", json!({ "topics": ["input"] }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn input_emit_is_the_engines_privilege_and_checks_its_shape() {
    let core = TestCore::start().await;
    let mut eng = engine(&core).await;

    // An interface holding every facade scope still cannot publish the topic:
    // it could otherwise fabricate a crossing or a refusal nobody made.
    let mut ui = interface(&core).await;
    let err = ui
        .request(
            "input.emit",
            json!({ "method": "input.updated", "params": {} }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");

    // A notification outside the namespace, the reserved name itself, or params
    // that are not an object: refused, nothing published.
    for bad in [
        json!({ "method": "devices.updated", "params": {} }),
        json!({ "method": "input.emit", "params": {} }),
        json!({ "method": "input.updated", "params": "not-an-object" }),
        json!({ "method": "input.updated" }),
    ] {
        let err = eng.request("input.emit", bad).await.unwrap_err();
        assert_eq!(err.code, -32602);
    }
}
