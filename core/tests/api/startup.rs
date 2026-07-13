// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Core startup: single instance per user, and the lifecycle of spawn tokens.
//! What the daemon needs to know before listening.

use crate::support::*;

#[tokio::test]
async fn a_second_core_refuses_the_endpoint_of_a_living_one() {
    let core = TestCore::start().await;
    let token_before = core.file_token();

    let err = core.start_rival().await;
    assert!(
        matches!(err, universallink_core::SpawnError::AlreadyRunning),
        "a second Core must recognize itself as one too many, not fail at random: {err}"
    );

    // The rival gave up BEFORE touching the file token. Without that ordering,
    // it would have revoked the living Core's secret out from under it: its
    // components would have reconnected with a token no one recognizes anymore.
    assert_eq!(
        core.file_token(),
        token_before,
        "the living Core's ipc-token was rewritten by a Core that did not even start"
    );

    // And the first is still listening.
    let mut c = core.connect().await;
    let r = c
        .hello("gui", "gui", &["session.read"], Some(&token_before))
        .await
        .expect("hello");
    assert_eq!(r["status"], "ok");
}

#[tokio::test]
async fn a_core_that_stopped_leaves_its_endpoint_free() {
    // The lock is released when the CoreHandle is dropped — not when the accept
    // task deigns to die. `restart()` depends on it, and so does the supervisor.
    let core = TestCore::start().await;
    let core = core.restart().await;

    let mut c = core.connect().await;
    let r = c
        .hello("gui", "gui", &["session.read"], Some(&core.file_token()))
        .await
        .expect("hello");
    assert_eq!(r["status"], "ok");
}

#[tokio::test]
async fn a_revoked_spawn_token_opens_nothing() {
    let core = TestCore::start().await;
    let token = core.mint("tray", &["session.read"]);
    core.handle.revoke_spawn_token(&token);

    // The token of a child that died before its hello must not outlive the
    // child: the supervisor reclaims it, and a relaunch mints a fresh one.
    let mut c = core.connect().await;
    let err = c
        .hello("tray-official", "tray", &["session.read"], Some(&token))
        .await
        .expect_err("a revoked token authenticates nothing anymore");
    assert_eq!(err.app_code(), "INVALID_TOKEN");
}

#[tokio::test]
async fn revoking_one_spawn_token_leaves_the_others() {
    let core = TestCore::start().await;
    let dead = core.mint("tray", &["session.read"]);
    let alive = core.mint("clipboard-backend", &["clipboard.read"]);
    core.handle.revoke_spawn_token(&dead);

    let mut c = core.connect().await;
    let r = c
        .hello(
            "clipboard",
            "clipboard-backend",
            &["clipboard.read"],
            Some(&alive),
        )
        .await
        .expect("hello");
    assert_eq!(r["status"], "ok");
}
