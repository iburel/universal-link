// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! `system.shutdown`: a component (the tray's Quit) asks the Core to stop. The
//! library only SIGNALS — the binary owns the teardown; here we check the gate
//! and the signal (doc/core-api.md, "system.*").

use std::time::Duration;

use serde_json::json;

use crate::support::*;

#[tokio::test]
async fn shutdown_requires_its_scope() {
    let core = TestCore::start().await;
    // A tray granted only `session.read` (the minimal profile) cannot stop the
    // Core: killing the daemon is a strictly stronger right than reading status.
    let mut tray = spawn_component(&core, "tray-official", "tray", &["session.read"]).await;

    let err = tray
        .request("system.shutdown", json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn shutdown_signals_the_binary() {
    let core = TestCore::start().await;
    let mut tray = spawn_component(&core, "tray-quit", "tray", &["system.shutdown"]).await;

    let r = tray
        .request("system.shutdown", json!({}))
        .await
        .expect("system.shutdown");
    assert_eq!(r, json!({}));

    // The library does not exit the process (that is the binary's job): it
    // signals. The signal is memorized (`notify_one`), so awaiting it AFTER the
    // request still resolves — a request that lands before the binary waits is
    // not lost.
    tokio::time::timeout(Duration::from_secs(1), core.handle.shutdown_requested())
        .await
        .expect("the Core recorded the shutdown request");
}
