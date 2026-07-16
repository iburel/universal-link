// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The data-channel client (`ConsumerChannel` / `ProviderChannel`) against the
//! real Core: file-range reads, the inline pull (`get_data` → provider channel
//! → `respond`), request-scoped vs terminal errors. This is the exact contract
//! the clipboard backend will consume.
//!
//! The FETCH test runs on the multi-threaded runtime: the fetch and the
//! provider are two data-channel connections that must make progress at the
//! same time — on a single-threaded runtime macOS's kqueue readiness ordering
//! can starve one of them (Linux's epoll happens not to).

use serde_json::json;
use tokio::sync::mpsc;
use universallink_ipc_client::{
    ChannelError, Client, ConsumerChannel, ErrorCode, Event, ProviderChannel,
};

use crate::support::*;

/// A connected `clipboard-backend` client, optionally serving `clipboard.get_data`.
async fn backend(core: &TestCore, served: &[&str]) -> (Client, mpsc::Receiver<Event>) {
    let scopes = ["clipboard.read", "clipboard.write"];
    let mut cfg = client_config(core, "clipboard-backend", &scopes, &[]);
    cfg.served_methods = served.iter().map(|s| s.to_string()).collect();
    let (client, mut events) = universallink_ipc_client::spawn(cfg);
    expect_connected(&mut events, &scopes).await;
    (client, events)
}

async fn open_consumer(client: &Client, core: &TestCore, tx_id: &str) -> ConsumerChannel {
    let token = client
        .request("transactions.open", json!({ "tx_id": tx_id }))
        .await
        .expect("transactions.open")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string();
    ConsumerChannel::open(&core.ipc_path(), &token)
        .await
        .expect("open consumer channel")
}

#[tokio::test(flavor = "multi_thread")]
async fn consumer_channel_reads_manifest_ranges() {
    let core = TestCore::start().await;
    let (client, _events) = backend(&core, &[]).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("data.bin");
    std::fs::write(&path, b"0123456789").expect("write file");

    let tx = client
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "files" }], "paths": [path.to_str().unwrap()] }),
        )
        .await
        .expect("clipboard.updated");
    let tx_id = tx["tx_id"].as_str().expect("tx_id").to_string();

    let mut ch = open_consumer(&client, &core, &tx_id).await;

    // The manifest names the first file f0 (Core convention). Ranges in any order.
    assert_eq!(ch.read("f0", 5, 3).await.expect("read"), b"567");
    assert_eq!(ch.read("f0", 0, 4).await.expect("read"), b"0123");
    // A read past the end returns the intersection (here: nothing) then EOF.
    assert_eq!(ch.read("f0", 100, 10).await.expect("read"), b"");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_read_resynchronizes_on_the_next_request() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let core = TestCore::start().await;
    let (client, _events) = backend(&core, &[]).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("data.bin");
    // Big enough that the response spans several DATA frames.
    let content: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
    std::fs::write(&path, &content).expect("write file");

    let tx = client
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "files" }], "paths": [path.to_str().unwrap()] }),
        )
        .await
        .expect("clipboard.updated");
    let tx_id = tx["tx_id"].as_str().expect("tx_id").to_string();
    let mut ch = open_consumer(&client, &core, &tx_id).await;

    // Poll a read exactly once — enough to write the READ request — then drop
    // it, abandoning the response mid-flight (a lost select! race).
    {
        let read = ch.read("f0", 0, 200_000);
        tokio::pin!(read);
        poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending(), "read completed in one poll");
            Poll::Ready(())
        })
        .await;
    }

    // The abandoned response is drained transparently: the next reads return
    // the right bytes, never the leftover frames.
    assert_eq!(ch.read("f0", 0, 4).await.expect("read"), content[0..4].to_vec());
    assert_eq!(
        ch.read("f0", 100, 4).await.expect("read"),
        content[100..104].to_vec()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn read_of_unknown_file_is_request_scoped() {
    let core = TestCore::start().await;
    let (client, _events) = backend(&core, &[]).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("data.bin");
    std::fs::write(&path, b"abcdef").expect("write file");

    let tx = client
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "files" }], "paths": [path.to_str().unwrap()] }),
        )
        .await
        .expect("clipboard.updated");
    let tx_id = tx["tx_id"].as_str().expect("tx_id").to_string();
    let mut ch = open_consumer(&client, &core, &tx_id).await;

    // An unknown file_id is a request-scoped error: the channel stays usable.
    match ch.read("does-not-exist", 0, 4).await {
        Err(ChannelError::Code(ErrorCode::FileUnknown)) => {}
        other => panic!("expected FILE_UNKNOWN, got {other:?}"),
    }
    assert_eq!(ch.read("f0", 0, 3).await.expect("read after error"), b"abc");
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_pulls_an_inline_blob_through_a_provider_channel() {
    let core = TestCore::start().await;
    let (client, mut events) = backend(&core, &["clipboard.get_data"]).await;

    let tx = client
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "text", "size": 13 }] }),
        )
        .await
        .expect("clipboard.updated");
    let tx_id = tx["tx_id"].as_str().expect("tx_id").to_string();
    let mut ch = open_consumer(&client, &core, &tx_id).await;

    let ipc = core.ipc_path();
    let provider_client = client.clone();
    // Serve the get_data request concurrently with the consumer's fetch.
    let serve = async {
        let (id, ptoken) = match next_event(&mut events).await {
            Event::Request { id, method, params } => {
                assert_eq!(method, "clipboard.get_data");
                assert_eq!(params["tx_id"], json!(tx_id));
                assert_eq!(params["format"], json!("text"));
                (
                    id,
                    params["channel_token"]
                        .as_str()
                        .expect("channel_token")
                        .to_string(),
                )
            }
            other => panic!("expected get_data, got {other:?}"),
        };
        let mut provider = ProviderChannel::open(&ipc, &ptoken)
            .await
            .expect("open provider");
        provider
            .data(0, b"pull-at-paste")
            .await
            .expect("provider data");
        provider.eof().await.expect("provider eof");
        // The reply follows EOF: it is the completion signal.
        provider_client
            .respond(id, json!({}))
            .await
            .expect("respond");
    };
    let (fetched, ()) = tokio::join!(ch.fetch("text"), serve);
    assert_eq!(fetched.expect("fetch"), b"pull-at-paste");
}

// ---------------------------------------------------------------------------
// ended(): a terminal error or a close pushed by the Core, exercised with a
// scripted data-channel server (binary frames after the attach frame). The
// real Core's proactive TX_STALE fires on a source logout/stop, which needs a
// full account; the wire behavior is what matters here.
// ---------------------------------------------------------------------------

/// A binary data-channel `ERROR { code }` frame: `[u32 BE len][0x12][json]`.
fn error_frame(code: &str) -> Vec<u8> {
    let payload = json!({ "code": code }).to_string().into_bytes();
    let len = (1 + payload.len()) as u32;
    let mut bytes = len.to_be_bytes().to_vec();
    bytes.push(0x12);
    bytes.extend_from_slice(&payload);
    bytes
}

#[tokio::test]
async fn ended_returns_a_terminal_error_pushed_on_the_channel() {
    let mut scripted = ScriptedCore::start().await;
    let path = scripted.path();
    let (ch, conn) = tokio::join!(ConsumerChannel::open(&path, "tok"), scripted.accept());
    let mut ch = ch.expect("open channel");
    let mut conn = conn;

    // The attach frame is one LSP frame carrying only the token.
    let attach = conn.recv().await;
    assert_eq!(attach["channel_token"], "tok");
    assert!(
        attach.get("method").is_none(),
        "attach frame carries no method"
    );

    // The Core pushes a terminal ERROR with no preceding request.
    conn.send_raw(&error_frame("TX_STALE")).await;
    match ch.ended().await {
        ChannelError::Code(code) => {
            assert_eq!(code, ErrorCode::TxStale);
            assert!(code.is_terminal());
        }
        other => panic!("expected TX_STALE, got {other:?}"),
    }
}

#[tokio::test]
async fn ended_returns_closed_when_the_channel_closes() {
    let mut scripted = ScriptedCore::start().await;
    let path = scripted.path();
    let (ch, conn) = tokio::join!(ConsumerChannel::open(&path, "tok"), scripted.accept());
    let ch = ch.expect("open channel");
    let mut ch = ch;
    let mut conn = conn;
    let _ = conn.recv().await; // attach frame

    // The Core drops the channel with no terminal error.
    drop(conn);
    assert!(matches!(ch.ended().await, ChannelError::Closed));
}
