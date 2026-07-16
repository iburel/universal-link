// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Clipboard transactions (doc/core-api.md, "Transactions" + "clipboard.*").
//!
//! Brick 1 (source control plane): `clipboard.updated` opens a transaction that
//! supersedes the previous clip; `clipboard.current` snapshots it. Announcing
//! is bound to the exclusive `clipboard-backend` role AND the `clipboard.write`
//! scope; the manifest is frozen from the paths (metadata only, no byte read).

use serde_json::json;

use crate::support::*;

/// The official clipboard backend: exclusive role, read + write scopes.
async fn backend(core: &TestCore) -> TestComponent {
    spawn_component(
        core,
        "clipboard",
        "clipboard-backend",
        &["clipboard.read", "clipboard.write"],
    )
    .await
}

#[tokio::test]
async fn announce_requires_the_backend_role_and_write_scope() {
    let core = TestCore::start().await;

    // Right scope, wrong role: a `custom` component holding `clipboard.write`
    // still cannot mint transactions — announcing is the exclusive backend's.
    let mut impostor =
        spawn_component(&core, "impostor", "custom", &["clipboard.write"]).await;
    let err = impostor
        .request("clipboard.updated", json!({ "formats": [{ "format": "text" }] }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");

    // Right role, no write scope (read-only): denied too.
    let mut reader =
        spawn_component(&core, "reader", "clipboard-backend", &["clipboard.read"]).await;
    let err = reader
        .request("clipboard.updated", json!({ "formats": [{ "format": "text" }] }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn announce_opens_a_transaction_reflected_by_current() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;

    // Nothing copied yet.
    let current = clip.request("clipboard.current", json!({})).await.unwrap();
    assert_eq!(current, json!({}));

    let r = clip
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "text", "size": 5 }] }),
        )
        .await
        .unwrap();
    let tx_id = r["tx_id"].as_str().expect("tx_id").to_string();
    assert!(tx_id.starts_with("tx_"), "unguessable tx_id: {tx_id}");

    let current = clip.request("clipboard.current", json!({})).await.unwrap();
    assert_eq!(current["tx_id"], json!(tx_id));
    assert_eq!(current["formats"], json!([{ "format": "text", "size": 5 }]));
}

#[tokio::test]
async fn a_new_announce_supersedes_the_previous_clip() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;

    let first = clip
        .request("clipboard.updated", json!({ "formats": [{ "format": "text" }] }))
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    let second = clip
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "image/png" }] }),
        )
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first, second, "each announce mints a fresh tx_id");

    // Last copier wins: the current clip is the second announce.
    let current = clip.request("clipboard.current", json!({})).await.unwrap();
    assert_eq!(current["tx_id"], json!(second));
    assert_eq!(current["formats"], json!([{ "format": "image/png" }]));
}

#[tokio::test]
async fn an_empty_announce_clears_the_clipboard() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;

    clip.request("clipboard.updated", json!({ "formats": [{ "format": "text" }] }))
        .await
        .unwrap();
    // Empty formats = the clipboard was cleared: a contentless transaction that
    // supersedes the previous one.
    let cleared = clip
        .request("clipboard.updated", json!({ "formats": [] }))
        .await
        .unwrap();
    assert!(cleared["tx_id"].as_str().is_some());

    let current = clip.request("clipboard.current", json!({})).await.unwrap();
    assert_eq!(current["tx_id"], cleared["tx_id"]);
    assert_eq!(current["formats"], json!([]));
    assert!(current.get("files").is_none());
}

#[tokio::test]
async fn a_files_announce_freezes_a_manifest() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;

    let a = core.write_source("report.pdf", b"PDF-1234");
    let b = core.write_source("notes.txt", b"hi");
    let r = clip
        .request(
            "clipboard.updated",
            json!({
                "formats": [{ "format": "files" }],
                "paths": [a.to_string_lossy(), b.to_string_lossy()],
            }),
        )
        .await
        .unwrap();
    assert!(r["tx_id"].as_str().is_some());

    let files = clip.request("clipboard.current", json!({})).await.unwrap()["files"].clone();
    let files = files.as_array().expect("manifest");
    assert_eq!(files.len(), 2);
    // The manifest exposes relative names + sizes, never the on-disk source
    // paths.
    assert_eq!(files[0]["path"], json!("report.pdf"));
    assert_eq!(files[0]["size"], json!(8));
    assert_eq!(files[1]["path"], json!("notes.txt"));
    assert_eq!(files[1]["size"], json!(2));
    assert!(files[0]["file_id"].as_str().is_some());
    // No source leak.
    let raw = clip.request("clipboard.current", json!({})).await.unwrap();
    assert!(!raw.to_string().contains("outbox"));
}

#[tokio::test]
async fn a_files_announce_refuses_a_directory() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;

    // v1 flat files: a directory in the paths is refused.
    let err = clip
        .request(
            "clipboard.updated",
            json!({
                "formats": [{ "format": "files" }],
                "paths": [core.config_dir().to_string_lossy()],
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn files_and_paths_must_agree() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;

    // `files` format but no paths.
    let err = clip
        .request("clipboard.updated", json!({ "formats": [{ "format": "files" }] }))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);

    // paths but no `files` format.
    let a = core.write_source("x.txt", b"x");
    let err = clip
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "text" }], "paths": [a.to_string_lossy()] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn current_requires_the_read_scope() {
    let core = TestCore::start().await;
    // A tray with only session.read cannot read the clipboard snapshot.
    let mut tray = spawn_component(&core, "tray", "tray", &["session.read"]).await;
    let err = tray.request("clipboard.current", json!({})).await.unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

// ---------------------------------------------------------------------------
// Brick 2: the data channel — consumer channels serving local file ranges.
// ---------------------------------------------------------------------------

/// Announces a single file and returns its `tx_id`.
async fn announce_file(core: &TestCore, clip: &mut TestComponent, name: &str, bytes: &[u8]) -> String {
    let path = core.write_source(name, bytes);
    clip.request(
        "clipboard.updated",
        json!({ "formats": [{ "format": "files" }], "paths": [path.to_string_lossy()] }),
    )
    .await
    .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_consumer_channel_reads_ranges_in_any_order() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    let tx = announce_file(&core, &mut clip, "data.bin", b"0123456789").await;

    let token = clip
        .request("transactions.open", json!({ "tx_id": tx }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ch = core.open_channel(&token).await;

    // Out of order, arbitrary ranges — as an OS surface reads a "local" file.
    assert_eq!(ch.read("f0", 5, 3).await.unwrap(), b"567");
    assert_eq!(ch.read("f0", 0, 4).await.unwrap(), b"0123");
    // The whole file.
    assert_eq!(ch.read("f0", 0, 10).await.unwrap(), b"0123456789");
    // Past the end: the intersection then EOF (a clamped read, never an error).
    assert_eq!(ch.read("f0", 8, 10).await.unwrap(), b"89");
    assert_eq!(ch.read("f0", 20, 5).await.unwrap(), b"");
}

#[tokio::test]
async fn reading_an_unknown_file_id_is_request_scoped() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    let tx = announce_file(&core, &mut clip, "data.bin", b"hello").await;
    let token = clip
        .request("transactions.open", json!({ "tx_id": tx }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ch = core.open_channel(&token).await;

    // Unknown id: FILE_UNKNOWN, but the channel survives for the next read.
    assert_eq!(ch.read("f9", 0, 5).await.unwrap_err(), "FILE_UNKNOWN");
    assert_eq!(ch.read("f0", 0, 5).await.unwrap(), b"hello");
}

#[tokio::test]
async fn a_changed_file_fails_the_read() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    let path = core.write_source("doc.txt", b"original");
    let tx = clip
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "files" }], "paths": [path.to_string_lossy()] }),
        )
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = clip
        .request("transactions.open", json!({ "tx_id": tx }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ch = core.open_channel(&token).await;

    // The frozen file is rewritten (different size) under our feet: the read is
    // refused rather than serving different bytes.
    std::fs::write(&path, b"tampered-and-longer").unwrap();
    assert_eq!(ch.read("f0", 0, 8).await.unwrap_err(), "FILE_CHANGED");
}

#[tokio::test]
async fn open_refuses_an_unknown_or_superseded_transaction() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;

    // Unknown tx_id.
    let err = clip
        .request("transactions.open", json!({ "tx_id": "tx_nope" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");

    // A superseded transaction accepts no NEW session.
    let first = announce_file(&core, &mut clip, "a.txt", b"aaa").await;
    announce_file(&core, &mut clip, "b.txt", b"bbb").await; // supersedes `first`
    let err = clip
        .request("transactions.open", json!({ "tx_id": first }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");
}

#[tokio::test]
async fn an_open_session_survives_supersession() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    let first = announce_file(&core, &mut clip, "a.txt", b"in-flight").await;

    let token = clip
        .request("transactions.open", json!({ "tx_id": first }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ch = core.open_channel(&token).await;
    // First read forces the session to be established before we supersede.
    assert_eq!(ch.read("f0", 0, 9).await.unwrap(), b"in-flight");

    // Copy something else: `first` is superseded, but the open paste runs to
    // completion — copying never cancels an in-flight paste.
    announce_file(&core, &mut clip, "b.txt", b"bbb").await;
    assert_eq!(ch.read("f0", 0, 9).await.unwrap(), b"in-flight");

    // No NEW session on the superseded clip, though.
    let err = clip
        .request("transactions.open", json!({ "tx_id": first }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");
}

#[tokio::test]
async fn a_channel_token_is_single_use() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    let tx = announce_file(&core, &mut clip, "a.txt", b"once").await;
    let token = clip
        .request("transactions.open", json!({ "tx_id": tx }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();

    let mut ch1 = core.open_channel(&token).await;
    assert_eq!(ch1.read("f0", 0, 4).await.unwrap(), b"once");
    // The token was consumed at the first attach: a second presentation is
    // turned away (the connection is closed).
    let mut ch2 = core.open_channel(&token).await;
    assert_eq!(ch2.read("f0", 0, 4).await.unwrap_err(), "closed");
}

#[tokio::test]
async fn open_requires_the_read_scope() {
    let core = TestCore::start().await;
    let mut tray = spawn_component(&core, "tray", "tray", &["session.read"]).await;
    let err = tray
        .request("transactions.open", json!({ "tx_id": "tx_x" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

// ---------------------------------------------------------------------------
// Brick 3: the inline path — FETCH pulled from the backend (pull-at-paste),
// streamed over a provider channel the Core relays to the consumer.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetch_pulls_an_inline_blob_from_the_backend() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;

    // The backend announces text (no bytes travel at the announce).
    let tx = clip
        .request("clipboard.updated", json!({ "formats": [{ "format": "text" }] }))
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = clip
        .request("transactions.open", json!({ "tx_id": tx.clone() }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ch = core.open_channel(&token).await;

    // The FETCH triggers a `clipboard.get_data` to the backend, which opens a
    // provider channel and streams the blob — run both sides concurrently.
    let fetch = ch.fetch("text");
    let serve = async {
        let (id, params) = clip.expect_request("clipboard.get_data").await;
        assert_eq!(params["tx_id"], json!(tx));
        assert_eq!(params["format"], json!("text"));
        let ptoken = params["channel_token"].as_str().unwrap().to_string();
        let mut provider = core.open_channel(&ptoken).await;
        provider.send_data(0, b"pull-at-paste").await;
        provider.send_eof().await;
        // The reply comes after EOF — the completion signal.
        clip.respond(id, json!({})).await;
    };
    let (fetched, ()) = tokio::join!(fetch, serve);
    assert_eq!(fetched.unwrap(), b"pull-at-paste");
}

#[tokio::test]
async fn fetch_of_an_absent_format_is_request_scoped() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    let tx = clip
        .request("clipboard.updated", json!({ "formats": [{ "format": "text" }] }))
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = clip
        .request("transactions.open", json!({ "tx_id": tx }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ch = core.open_channel(&token).await;

    // Not announced → FORMAT_UNKNOWN, without even contacting the backend; the
    // channel survives.
    assert_eq!(ch.fetch("image/png").await.unwrap_err(), "FORMAT_UNKNOWN");
    // And a genuine text FETCH still works afterwards.
    let fetch = ch.fetch("text");
    let serve = async {
        let (id, params) = clip.expect_request("clipboard.get_data").await;
        let mut provider = core.open_channel(params["channel_token"].as_str().unwrap()).await;
        provider.send_data(0, b"ok").await;
        provider.send_eof().await;
        clip.respond(id, json!({})).await;
    };
    let (fetched, ()) = tokio::join!(fetch, serve);
    assert_eq!(fetched.unwrap(), b"ok");
}

#[tokio::test]
async fn a_stale_clipboard_fails_the_fetch() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    let tx = clip
        .request("clipboard.updated", json!({ "formats": [{ "format": "text" }] }))
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = clip
        .request("transactions.open", json!({ "tx_id": tx }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ch = core.open_channel(&token).await;

    // The backend can no longer vouch for the generation (the OS clipboard
    // moved on): it refuses with CLIP_STALE, without opening a provider channel.
    let fetch = ch.fetch("text");
    let serve = async {
        let (id, _params) = clip.expect_request("clipboard.get_data").await;
        clip.respond_error(id, "CLIP_STALE").await;
    };
    let (fetched, ()) = tokio::join!(fetch, serve);
    assert_eq!(fetched.unwrap_err(), "CLIP_STALE");
}

#[tokio::test]
async fn fetch_when_the_announcer_is_gone_is_clip_stale() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    // A clip with both a file (to anchor the session before we cut the
    // announcer) and an inline format (the FETCH under test).
    let path = core.write_source("both.txt", b"filedata");
    let tx = clip
        .request(
            "clipboard.updated",
            json!({
                "formats": [{ "format": "files" }, { "format": "text" }],
                "paths": [path.to_string_lossy()],
            }),
        )
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = clip
        .request("transactions.open", json!({ "tx_id": tx }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ch = core.open_channel(&token).await;
    // A file read establishes the session (and confirms the attach consumed the
    // token) before the announcer leaves.
    assert_eq!(ch.read("f0", 0, 8).await.unwrap(), b"filedata");

    // The announcing backend disconnects: no one can re-read the OS clipboard
    // for this generation → CLIP_STALE. Files, by contrast, still read from disk.
    drop(clip);
    assert_eq!(ch.fetch("text").await.unwrap_err(), "CLIP_STALE");
    assert_eq!(ch.read("f0", 0, 8).await.unwrap(), b"filedata");
}

#[tokio::test]
async fn a_provider_that_closes_without_eof_yields_clip_stale() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    let tx = clip
        .request("clipboard.updated", json!({ "formats": [{ "format": "text" }] }))
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    let token = clip
        .request("transactions.open", json!({ "tx_id": tx }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ch = core.open_channel(&token).await;

    // The backend streams a partial blob then drops the provider channel WITHOUT
    // an EOF, and only then replies. The Core must settle on CLIP_STALE promptly
    // (it must NOT spin until the fetch timeout — this test would then hit the
    // per-message response timeout).
    let fetch = ch.fetch("text");
    let serve = async {
        let (id, params) = clip.expect_request("clipboard.get_data").await;
        let mut provider = core.open_channel(params["channel_token"].as_str().unwrap()).await;
        provider.send_data(0, b"partial").await;
        drop(provider); // close without EOF
        clip.respond(id, json!({})).await;
    };
    let (fetched, ()) = tokio::join!(fetch, serve);
    assert_eq!(fetched.unwrap_err(), "CLIP_STALE");
}

#[tokio::test]
async fn logout_drops_transactions_and_cuts_sessions() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut clip = backend(&core).await;
    let tx = announce_file(&core, &mut clip, "secret.txt", b"secret").await;
    let token = clip
        .request("transactions.open", json!({ "tx_id": tx }))
        .await
        .unwrap()["channel_token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ch = core.open_channel(&token).await;
    assert_eq!(ch.read("f0", 0, 6).await.unwrap(), b"secret");

    // A component logs the account out. Its read grants do not outlive the
    // session: the open channel is cut and further reads fail TX_STALE.
    let mut mgr = spawn_component(&core, "mgr", "custom", &["session.manage"]).await;
    mgr.request("session.logout", json!({})).await.unwrap();

    assert_eq!(ch.read("f0", 0, 6).await.unwrap_err(), "TX_STALE");
}
