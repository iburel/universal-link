// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The engine behind the routed facade: the shipping profile connects, the
//! snapshot answers, the not-yet-served vocabulary refuses honestly, and the
//! supervised-component contract's stop signal ends the loop cleanly.

use onedevice_ipc_client::RequestError;
use onedevice_sync::Outcome;
use serde_json::json;

use crate::support::{Engine, TestCore, status_eventually, ui};

/// The shipping role/scope profile enrolls, and `sync.status` crosses the
/// facade with the honestly empty snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn the_engine_serves_the_empty_snapshot_through_the_facade() {
    let core = TestCore::start().await;
    let engine = Engine::start(&core);
    let ui = ui(&core).await;

    let status = status_eventually(&ui).await;
    assert_eq!(status, json!({ "sets": [], "invitations": [] }));

    assert_eq!(engine.stop().await, Outcome::StdinClosed);
}

/// A `sync.*` name outside the frozen vocabulary is refused with `-32601`
/// by the engine's OWN client (the served-methods gate), and the Core relays
/// that refusal verbatim - not `COMPONENT_ABSENT`: the engine is there, the
/// method is not.
#[tokio::test(flavor = "multi_thread")]
async fn a_gesture_outside_the_served_list_relays_method_not_found() {
    let core = TestCore::start().await;
    let engine = Engine::start(&core);
    let ui = ui(&core).await;

    // Prove the engine is serving first, so the refusal below cannot be a
    // race with its hello.
    status_eventually(&ui).await;

    match ui
        .client
        .request("sync.imagine", json!({ "set_id": "s" }))
        .await
    {
        Err(RequestError::Rpc(e)) => {
            assert_eq!(e.code, -32601, "expected method-not-found, got: {e:?}");
        }
        other => panic!("expected the relayed -32601, got: {other:?}"),
    }

    assert_eq!(engine.stop().await, Outcome::StdinClosed);
}
