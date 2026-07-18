// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Source side against a REAL Core: the orchestrator announces local copies,
//! supersedes them, and serves an inline paste over a provider channel. A
//! second client (role `custom`, `clipboard.read`) plays the pasting device and
//! learns the `tx_id` through `clipboard.current`.
//!
//! Multi-threaded runtime: the provider serve and the consumer fetch are two
//! data-channel connections that must make progress at the same time (as in the
//! client crate's channel suite).

use serde_json::json;
use universallink_clipboard::{BackendEvent, Format, LocalClip, run};
use universallink_ipc_client::{ChannelError, ConsumerChannel, ErrorCode};

use crate::support::*;

fn inline_clip(format: &str, size: Option<u64>) -> LocalClip {
    LocalClip {
        formats: vec![Format {
            id: format.into(),
            size,
        }],
        paths: Vec::new(),
        sensitive: false,
    }
}

/// Spawns the orchestrator over `backend`, returning the fake backend, the
/// backend-event sender, and the Core's IPC path.
fn spawn_orchestrator(
    core: &TestCore,
    mut backend: Backend,
) -> (
    FakeBackend,
    tokio::sync::mpsc::Sender<BackendEvent>,
    std::path::PathBuf,
) {
    let fake = backend.fake.clone();
    let backend_tx = backend.backend_tx.clone();
    let backend_rx = backend.backend_rx.take().expect("backend_rx");
    let ipc_path = core.ipc_path();
    tokio::spawn(run(
        backend.client,
        backend.events,
        fake.clone(),
        ipc_path.clone(),
        backend_rx,
        never(),
    ));
    (fake, backend_tx, ipc_path)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_copy_opens_a_transaction() {
    let core = TestCore::start().await;
    let backend = Backend::connect(&core).await;
    let (_fake, backend_tx, _ipc) = spawn_orchestrator(&core, backend);
    let consumer = Consumer::connect(&core).await;

    backend_tx
        .send(BackendEvent::Copied {
            generation: 1,
            clip: inline_clip("text", Some(5)),
        })
        .await
        .expect("send Copied");

    let tx_id = consumer.await_current_tx(&["text"]).await;
    assert!(
        consumer.open(&tx_id).await.is_ok(),
        "the announced transaction must be openable"
    );
}

/// The orchestrator serves an inline `get_data` over a provider channel and
/// replies only after EOF. Driven against a scripted Core, so the serve is a
/// single data-channel connection (the provider). The real-Core end-to-end
/// relay of a provider blob down to a consumer is covered by the client crate's
/// `channel` suite; forcing both concurrent data channels through the real Core
/// is the macOS kqueue-starvation pattern that suite is written to handle.
#[tokio::test(flavor = "multi_thread")]
async fn serves_get_data_over_a_provider_channel() {
    let (mut scripted, mut conn, backend_tx, fake) = scripted_orchestrator().await;

    // Announce a local copy the backend can vouch for.
    fake.provision(1, "text", b"pull-at-paste");
    backend_tx
        .send(BackendEvent::Copied {
            generation: 1,
            clip: inline_clip("text", Some(13)),
        })
        .await
        .expect("send Copied");
    let params = conn
        .handle_request("clipboard.updated", json!({ "tx_id": "tx-1" }))
        .await;
    assert_eq!(params["formats"], json!([{ "format": "text", "size": 13 }]));

    // A remote device pastes an inline format: the Core asks the backend for it.
    conn.send(&json!({
        "jsonrpc": "2.0",
        "id": 100,
        "method": "clipboard.get_data",
        "params": { "tx_id": "tx-1", "format": "text", "channel_token": "pt-1" },
    }))
    .await;

    // The orchestrator opens a provider channel and streams DATA then EOF.
    let mut provider = scripted.accept().await;
    assert_eq!(provider.recv_attach().await, "pt-1");
    let (data_tag, payload) = provider.recv_binary().await;
    assert_eq!(data_tag, 0x10, "DATA frame");
    assert_eq!(
        &payload[8..],
        b"pull-at-paste",
        "DATA payload after the u64 offset"
    );
    let (eof_tag, _) = provider.recv_binary().await;
    assert_eq!(eof_tag, 0x11, "EOF frame");

    // Only after EOF does the reply come back — it is the completion signal.
    let reply = conn.recv().await;
    assert_eq!(reply["id"], 100);
    assert!(
        reply.get("result").is_some(),
        "get_data replies a result after EOF, got {reply}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paste_the_backend_cannot_vouch_for_is_clip_stale() {
    let core = TestCore::start().await;
    let backend = Backend::connect(&core).await;
    // The fake backend is NOT provisioned: `provide` returns `None`.
    let (_fake, backend_tx, ipc) = spawn_orchestrator(&core, backend);
    let consumer = Consumer::connect(&core).await;

    backend_tx
        .send(BackendEvent::Copied {
            generation: 1,
            clip: inline_clip("text", Some(3)),
        })
        .await
        .expect("send Copied");

    let tx_id = consumer.await_current_tx(&["text"]).await;
    let channel_token = consumer.open(&tx_id).await.expect("open");
    let mut channel = ConsumerChannel::open(&ipc, &channel_token)
        .await
        .expect("open consumer channel");

    match channel.fetch("text").await {
        Err(ChannelError::Code(ErrorCode::ClipStale)) => {}
        other => panic!("expected CLIP_STALE, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn clearing_supersedes_the_transaction() {
    let core = TestCore::start().await;
    let backend = Backend::connect(&core).await;
    let (_fake, backend_tx, _ipc) = spawn_orchestrator(&core, backend);
    let consumer = Consumer::connect(&core).await;

    backend_tx
        .send(BackendEvent::Copied {
            generation: 1,
            clip: inline_clip("text", Some(4)),
        })
        .await
        .expect("send Copied");
    let tx_id = consumer.await_current_tx(&["text"]).await;

    backend_tx
        .send(BackendEvent::Cleared)
        .await
        .expect("send Cleared");
    consumer.await_cleared().await;

    // The cleared clipboard superseded the transaction: no new session.
    assert_eq!(consumer.open(&tx_id).await, Err("TX_STALE".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_copy_supersedes_the_previous_one() {
    let core = TestCore::start().await;
    let backend = Backend::connect(&core).await;
    let (_fake, backend_tx, _ipc) = spawn_orchestrator(&core, backend);
    let consumer = Consumer::connect(&core).await;

    backend_tx
        .send(BackendEvent::Copied {
            generation: 1,
            clip: inline_clip("text", Some(4)),
        })
        .await
        .expect("send Copied");
    let tx_a = consumer.await_current_tx(&["text"]).await;

    backend_tx
        .send(BackendEvent::Copied {
            generation: 2,
            clip: inline_clip("image/png", None),
        })
        .await
        .expect("send Copied");
    let tx_b = consumer.await_current_tx(&["image/png"]).await;

    assert_ne!(tx_a, tx_b, "a new copy mints a fresh transaction");
    assert_eq!(
        consumer.open(&tx_a).await,
        Err("TX_STALE".to_string()),
        "the superseded transaction refuses new sessions"
    );
    assert!(
        consumer.open(&tx_b).await.is_ok(),
        "the current transaction is openable"
    );
}
