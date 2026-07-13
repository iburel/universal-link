// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! `core_request`: full proxy, faithful relay of errors, fail-closed when
//! offline, malformed arguments turned away.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::support::*;

#[tokio::test(flavor = "multi_thread")]
async fn results_and_rpc_errors_are_relayed() {
    let core = TestCore::start().await;
    let shell = shell_app(gui_config(&core)).await;
    shell.wait_status("connected").await;

    let r = shell
        .core_request("session.status", json!({}))
        .await
        .expect("session.status");
    assert_eq!(r["logged_in"], false);
    assert_eq!(r["server_connected"], false);

    // `params` is optional: omitted on the frontend side = `{}` on the Core side.
    let r = shell
        .invoke("core_request", json!({ "method": "session.status" }))
        .await
        .expect("core_request without params");
    assert_eq!(r["logged_in"], false);

    // Application error relayed as-is (no server configured).
    let e = shell
        .core_request("devices.list", json!({}))
        .await
        .unwrap_err();
    assert_eq!(e["kind"], "rpc");
    assert_eq!(e["data_code"], "SERVER_UNREACHABLE");
    assert!(e["message"].as_str().is_some_and(|m| !m.is_empty()), "{e}");

    // Pure JSON-RPC error: unknown method, no application code.
    let e = shell
        .core_request("nope.nope", json!({}))
        .await
        .unwrap_err();
    assert_eq!(e["kind"], "rpc");
    assert_eq!(e["code"], -32601);
    assert!(e["data_code"].is_null(), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_fail_fast_without_connection() {
    let mut core = TestCore::start().await;
    let shell = shell_app(gui_config(&core)).await;
    shell.wait_status("connected").await;
    core.stop();
    shell.wait_status("connecting").await;

    let start = Instant::now();
    let e = shell
        .core_request("session.status", json!({}))
        .await
        .unwrap_err();
    assert_eq!(e["kind"], "not_connected");
    assert!(e["message"].as_str().is_some_and(|m| !m.is_empty()), "{e}");
    // Immediate: no waiting for a request timeout.
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "not_connected must be immediate ({:?})",
        start.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_invokes_are_rejected() {
    let core = TestCore::start().await;
    let shell = shell_app(gui_config(&core)).await;

    // `method` missing: argument deserialization refused.
    let e = shell
        .invoke("core_request", json!({ "params": {} }))
        .await
        .unwrap_err();
    let msg = e.as_str().unwrap_or_else(|| panic!("text error: {e}"));
    assert!(msg.contains("method"), "{msg}");

    // Unknown command: rejected, no panic.
    let e = shell.invoke("nope", json!({})).await.unwrap_err();
    let msg = e.as_str().unwrap_or_else(|| panic!("text error: {e}"));
    assert!(msg.contains("nope"), "{msg}");
}
