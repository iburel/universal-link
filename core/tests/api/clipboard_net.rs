// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Clipboard network plane (doc/core-api.md, "Transactions" — network mapping):
//! two Cores of the same account, over a memory transport, share the clipboard.
//!
//! Brick 4 (propagation): a local `clipboard.updated` reaches the other Core as
//! `clipboard.remote_updated`, with global last-copier-wins convergence and a
//! fail-closed re-validation of the manifest.
//!
//! Brick 5 (byte relay): a consumer channel opened on a REMOTE transaction
//! relays its `READ`s (file ranges from the source's disk) and `FETCH`es (inline
//! blobs pulled from the source's backend) over a `clip_session` stream — a
//! remote paste byte-identical to a local one. A source that stops gracefully
//! cuts with `TX_STALE`; one that vanishes surfaces as `PEER_GONE`.
//!
//! Brick 6 (`transactions.fill`): the Core writes designated target files
//! itself, from a remote source or the local disk, tracked via `transfer.*`.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use universallink_core::{PeerAddr, PeerTransport};
use universallink_test_support::memory_transport::{MemorySwitchboard, MemoryTransport};

use crate::support::*;

/// A clipboard backend with the scopes the network tests need: announce/read,
/// plus directory + session (reachability waits) and transfers (fill events).
async fn backend(core: &TestCore) -> TestComponent {
    spawn_component(
        core,
        "clipboard",
        "clipboard-backend",
        &[
            "clipboard.read",
            "clipboard.write",
            "devices.read",
            "session.read",
            "transfers.read",
        ],
    )
    .await
}

async fn subscribe(c: &mut TestComponent) {
    c.request(
        "events.subscribe",
        json!({ "topics": ["clipboard", "transfers"] }),
    )
    .await
    .expect("events.subscribe");
}

/// Two Cores of the same account, each with a subscribed backend, connected and
/// mutually reachable (attestation + relay both ways — the clipboard plane opens
/// streams in BOTH directions: A→B to announce, B→A to serve a paste).
async fn connected_pair(server: &TestServer) -> (TestCore, TestComponent, TestCore, TestComponent) {
    let (a, b) = TestCore::start_pair(server).await;
    let mut ca = backend(&a).await;
    let mut cb = backend(&b).await;
    subscribe(&mut ca).await;
    subscribe(&mut cb).await;
    wait_server_connected(&mut ca, true).await;
    wait_server_connected(&mut cb, true).await;
    wait_reachable(&mut ca, b.device_id()).await;
    wait_reachable(&mut cb, a.device_id()).await;
    (a, ca, b, cb)
}

/// Announces `text` from a backend and returns the `tx_id`.
async fn announce_text(c: &mut TestComponent, text: &str) -> String {
    c.request(
        "clipboard.updated",
        json!({ "formats": [{ "format": "text", "size": text.len() }] }),
    )
    .await
    .expect("clipboard.updated")["tx_id"]
        .as_str()
        .expect("tx_id")
        .to_string()
}

/// Announces a MATERIALIZED (push-at-copy) text clip: the inline bytes travel
/// with the announce. Returns the `tx_id`.
async fn announce_text_materialized(c: &mut TestComponent, text: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    c.request(
        "clipboard.updated",
        json!({
            "formats": [{ "format": "text" }],
            "materialize": true,
            "blobs": { "text": b64 },
        }),
    )
    .await
    .expect("clipboard.updated")["tx_id"]
        .as_str()
        .expect("tx_id")
        .to_string()
}

/// Announces a `files` clip from `paths` and returns the `tx_id`.
async fn announce_files(c: &mut TestComponent, paths: &[std::path::PathBuf]) -> String {
    let paths: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    c.request(
        "clipboard.updated",
        json!({ "formats": [{ "format": "files" }], "paths": paths }),
    )
    .await
    .expect("clipboard.updated")["tx_id"]
        .as_str()
        .expect("tx_id")
        .to_string()
}

// ---------------------------------------------------------------------------
// Brick 4: propagation + convergence.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_propagates_to_the_other_core() {
    let server = TestServer::start().await;
    let (a, mut ca, _b, mut cb) = connected_pair(&server).await;

    let tx = announce_text(&mut ca, "hello").await;

    // B's backend learns the copy — metadata only, attributed to A.
    let note = cb.wait_notification("clipboard.remote_updated").await;
    assert_eq!(note["tx_id"], json!(tx));
    assert_eq!(note["device_id"], json!(a.device_id()));
    assert_eq!(note["formats"], json!([{ "format": "text", "size": 5 }]));

    // And B's snapshot reflects the remote clip.
    let current = cb.request("clipboard.current", json!({})).await.unwrap();
    assert_eq!(current["tx_id"], json!(tx));
    assert_eq!(current["device_id"], json!(a.device_id()));
}

#[tokio::test(flavor = "multi_thread")]
async fn last_copier_wins_across_cores() {
    let server = TestServer::start().await;
    let (_a, mut ca, b, mut cb) = connected_pair(&server).await;

    // A copies; B learns it and adopts it as current.
    let tx_a = announce_text(&mut ca, "from A").await;
    let n = cb.wait_notification("clipboard.remote_updated").await;
    assert_eq!(n["tx_id"], json!(tx_a));

    // B copies: the fresher copy supersedes globally, and A converges onto it.
    let tx_b = announce_text(&mut cb, "from B").await;
    let n = ca.wait_notification("clipboard.remote_updated").await;
    assert_eq!(n["tx_id"], json!(tx_b));
    assert_eq!(n["device_id"], json!(b.device_id()));

    // Both Cores elect the same winner (B's copy).
    for c in [&mut ca, &mut cb] {
        let cur = c.request("clipboard.current", json!({})).await.unwrap();
        assert_eq!(cur["tx_id"], json!(tx_b));
        assert_eq!(cur["device_id"], json!(b.device_id()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_announce_never_overrides_a_fresher_clip() {
    let server = TestServer::start().await;
    let (b, mut cb, raw) = core_with_raw_peer(&server).await;

    // B makes a fresh local copy (a large, real-time `seq`).
    let tx_b = announce_text(&mut cb, "fresh local").await;

    // A raw peer forges an announce with an ancient `seq` (1): B refuses to
    // regress — last-copier-wins is by `(seq, device_id)`, so the stale copy is
    // ignored and the current clip is unchanged.
    raw.send_clip_announce(
        &b,
        json!({
            "tx_id": "tx_stale",
            "device_id": raw.device_id,
            "seq": 1,
            "formats": [{ "format": "text" }],
        }),
    )
    .await;

    // No `remote_updated`, current still B's copy.
    cb.assert_silent().await;
    assert_eq!(
        cb.request("clipboard.current", json!({})).await.unwrap()["tx_id"],
        json!(tx_b)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reused_tx_id_from_a_peer_is_rejected() {
    let server = TestServer::start().await;
    let (b, mut cb, raw) = core_with_raw_peer(&server).await;

    // A first announce establishes tx_reuse as a text clip.
    raw.send_clip_announce(
        &b,
        json!({
            "tx_id": "tx_reuse",
            "device_id": raw.device_id,
            "seq": 100,
            "formats": [{ "format": "text" }],
        }),
    )
    .await;
    let n = cb.wait_notification("clipboard.remote_updated").await;
    assert_eq!(n["tx_id"], json!("tx_reuse"));
    assert_eq!(n["formats"], json!([{ "format": "text" }]));

    // Reusing the SAME tx_id (even with a higher seq and different content) is
    // refused — a fresh copy must mint a fresh id, so a peer cannot clobber a
    // live transaction by naming it.
    raw.send_clip_announce(
        &b,
        json!({
            "tx_id": "tx_reuse",
            "device_id": raw.device_id,
            "seq": 200,
            "formats": [{ "format": "image/png" }],
        }),
    )
    .await;
    cb.assert_silent().await;
    let cur = cb.request("clipboard.current", json!({})).await.unwrap();
    assert_eq!(cur["tx_id"], json!("tx_reuse"));
    assert_eq!(cur["formats"], json!([{ "format": "text" }]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malicious_manifest_is_dropped_fail_closed() {
    let server = TestServer::start().await;
    let (b, mut cb, raw) = core_with_raw_peer(&server).await;

    // A crafted announce whose manifest tries to climb out of the paste target.
    // B's receiver re-validates fail-closed and drops it: no `remote_updated`,
    // and nothing becomes the current clip.
    raw.send_clip_announce(
        &b,
        json!({
            "tx_id": "tx_evil",
            "device_id": raw.device_id,
            "seq": 999,
            "formats": [{ "format": "files" }],
            "files": [{ "file_id": "f0", "path": "../../escape", "size": 3 }],
        }),
    )
    .await;

    cb.assert_silent().await;
    assert_eq!(
        cb.request("clipboard.current", json!({})).await.unwrap(),
        json!({})
    );
}

// ---------------------------------------------------------------------------
// Brick 5: the byte relay.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_file_reads_over_the_network() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = connected_pair(&server).await;

    let path = a.write_source("data.bin", b"0123456789");
    let tx = announce_files(&mut ca, &[path]).await;
    let note = cb.wait_notification("clipboard.remote_updated").await;
    assert_eq!(note["tx_id"], json!(tx));
    assert_eq!(note["files"][0]["path"], json!("data.bin"));
    assert_eq!(note["files"][0]["size"], json!(10));

    // B pastes: a consumer channel on the remote clip, reads relayed from A.
    let token = open_channel_token(&mut cb, &tx).await;
    let mut ch = b.open_channel(&token).await;

    // Arbitrary ranges, out of order, on the SAME session — as an OS surface
    // reads a "local" file.
    assert_eq!(ch.read("f0", 0, 10).await.unwrap(), b"0123456789");
    assert_eq!(ch.read("f0", 5, 3).await.unwrap(), b"567");
    // Past the end: the intersection then EOF (clamped, never an error).
    assert_eq!(ch.read("f0", 20, 5).await.unwrap(), b"");
    // Unknown id: request-scoped over the wire too — the channel survives.
    assert_eq!(ch.read("f9", 0, 5).await.unwrap_err(), "FILE_UNKNOWN");
    assert_eq!(ch.read("f0", 0, 4).await.unwrap(), b"0123");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_inline_blob_relays_from_the_source_backend() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = connected_pair(&server).await;

    let tx = announce_text(&mut ca, "unused-size").await;
    cb.wait_notification("clipboard.remote_updated").await;

    let token = open_channel_token(&mut cb, &tx).await;
    let mut ch = b.open_channel(&token).await;

    // B FETCHes the inline blob; the pull crosses to A, whose backend serves it
    // over its provider channel — the Core relays those bytes back to B.
    let fetch = ch.fetch("text");
    let serve = async {
        let (id, params) = ca.expect_request("clipboard.get_data").await;
        assert_eq!(params["tx_id"], json!(tx));
        assert_eq!(params["format"], json!("text"));
        let ptoken = params["channel_token"].as_str().unwrap();
        let mut provider = a.open_channel(ptoken).await;
        provider.send_data(0, b"across the wire").await;
        provider.send_eof().await;
        // The reply follows EOF — the completion signal.
        ca.respond(id, json!({})).await;
    };
    let (fetched, ()) = tokio::join!(fetch, serve);
    assert_eq!(fetched.unwrap(), b"across the wire");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_paste_survives_supersession_on_both_cores() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = connected_pair(&server).await;

    let f1 = a.write_source("first.bin", b"first-clip");
    let tx1 = announce_files(&mut ca, &[f1]).await;
    cb.wait_notification("clipboard.remote_updated").await;
    let token = open_channel_token(&mut cb, &tx1).await;
    let mut ch = b.open_channel(&token).await;
    // Establish the paste session (opens the clip_session stream, counted as a
    // session on BOTH Cores).
    assert_eq!(ch.read("f0", 0, 10).await.unwrap(), b"first-clip");

    // A copies something else: tx1 is superseded on A and — via propagation — on
    // B, but neither cuts the in-flight paste.
    let f2 = a.write_source("second.bin", b"second");
    let tx2 = announce_files(&mut ca, &[f2]).await;
    let note = cb.wait_notification("clipboard.remote_updated").await;
    assert_eq!(note["tx_id"], json!(tx2));

    // The open paste still reads its frozen manifest — copying never cancels it.
    assert_eq!(ch.read("f0", 0, 10).await.unwrap(), b"first-clip");

    // But a NEW session on the (now superseded) tx1 is refused.
    let err = cb
        .request("transactions.open", json!({ "tx_id": tx1 }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_logout_cuts_an_open_remote_session() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = connected_pair(&server).await;

    let path = a.write_source("s.bin", b"secret!!");
    let tx = announce_files(&mut ca, &[path]).await;
    cb.wait_notification("clipboard.remote_updated").await;
    let token = open_channel_token(&mut cb, &tx).await;
    let mut ch = b.open_channel(&token).await;
    // Establish the session before the source leaves.
    assert_eq!(ch.read("f0", 0, 8).await.unwrap(), b"secret!!");

    // A logs out: its read grants end NOW (not the graceful supersession) — the
    // open remote session is cut with TX_STALE, cross-Core.
    let mut mgr = spawn_component(&a, "mgr", "custom", &["session.manage"]).await;
    mgr.request("session.logout", json!({})).await.unwrap();
    assert_eq!(ch.read("f0", 0, 8).await.unwrap_err(), "TX_STALE");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vanished_source_surfaces_as_peer_gone() {
    let server = TestServer::start().await;
    let (b, mut cb, raw) = core_with_raw_peer(&server).await;

    // A source announces a file clip, then — unlike a graceful stop, which would
    // send TX_STALE — vanishes mid-session (a network partition: the device is
    // alive but unreachable, no terminal frame). B's relay surfaces PEER_GONE.
    raw.send_clip_announce(
        &b,
        json!({
            "tx_id": "tx_remote",
            "device_id": raw.device_id,
            "seq": 500,
            "formats": [{ "format": "files" }],
            "files": [{ "file_id": "f0", "path": "ghost.bin", "size": 8 }],
        }),
    )
    .await;
    let note = cb.wait_notification("clipboard.remote_updated").await;
    assert_eq!(note["tx_id"], json!("tx_remote"));

    let token = open_channel_token(&mut cb, "tx_remote").await;
    let mut ch = b.open_channel(&token).await;

    // B pastes; the source accepts the session stream then drops it abruptly.
    let read = ch.read("f0", 0, 8);
    let vanish = raw.abandon_next_session();
    let (res, ()) = tokio::join!(read, vanish);
    assert_eq!(res.unwrap_err(), "PEER_GONE");
}

// ---------------------------------------------------------------------------
// Brick 6: transactions.fill.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn fill_writes_remote_files_to_disk() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = connected_pair(&server).await;

    let f1 = a.write_source("one.txt", b"first file");
    let f2 = a.write_source("two.txt", b"the second file!!");
    let tx = announce_files(&mut ca, &[f1, f2]).await;
    let note = cb.wait_notification("clipboard.remote_updated").await;
    let files = note["files"].as_array().unwrap();
    let id1 = files[0]["file_id"].as_str().unwrap().to_string();
    let id2 = files[1]["file_id"].as_str().unwrap().to_string();

    // B designates paste-skeleton paths; the Core fills them from A.
    let dest1 = b.config_dir().join("paste-one.txt");
    let dest2 = b.config_dir().join("sub").join("paste-two.txt"); // missing parent created
    let r = cb
        .request(
            "transactions.fill",
            json!({
                "tx_id": tx,
                "entries": [
                    { "file_id": id1, "dest_path": dest1.to_string_lossy() },
                    { "file_id": id2, "dest_path": dest2.to_string_lossy() },
                ],
            }),
        )
        .await
        .unwrap();
    let transfer_id = r["transfer_id"].as_str().expect("transfer_id").to_string();

    let started = cb.wait_notification("transfer.started").await;
    assert_eq!(started["transfer_id"], json!(transfer_id));
    assert_eq!(started["device_id"], json!(a.device_id()));
    let finished = cb.wait_notification("transfer.finished").await;
    assert_eq!(finished["transfer_id"], json!(transfer_id));
    assert_eq!(finished["paths"].as_array().unwrap().len(), 2);

    assert_eq!(std::fs::read(&dest1).unwrap(), b"first file");
    assert_eq!(std::fs::read(&dest2).unwrap(), b"the second file!!");
}

#[tokio::test]
async fn fill_writes_local_files_to_disk() {
    // A local paste (fill on the same device that copied): no network, bytes
    // copied straight from the disk.
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    clip.request("events.subscribe", json!({ "topics": ["transfers"] }))
        .await
        .unwrap();

    let src = core.write_source("local.txt", b"local bytes");
    let tx = announce_files(&mut clip, &[src]).await;
    let cur = clip.request("clipboard.current", json!({})).await.unwrap();
    let file_id = cur["files"][0]["file_id"].as_str().unwrap().to_string();

    let dest = core.config_dir().join("out.txt");
    clip.request(
        "transactions.fill",
        json!({ "tx_id": tx, "entries": [{ "file_id": file_id, "dest_path": dest.to_string_lossy() }] }),
    )
    .await
    .unwrap();

    let finished = clip.wait_notification("transfer.finished").await;
    assert!(finished["transfer_id"].is_string());
    assert_eq!(std::fs::read(&dest).unwrap(), b"local bytes");
}

#[tokio::test]
async fn fill_refuses_an_unknown_file_id_or_transaction() {
    let core = TestCore::start().await;
    let mut clip = backend(&core).await;
    let src = core.write_source("x.txt", b"x");
    let tx = announce_files(&mut clip, &[src]).await;

    // Unknown file_id in a known transaction: invalid params.
    let err = clip
        .request(
            "transactions.fill",
            json!({ "tx_id": tx, "entries": [{ "file_id": "f9", "dest_path": "/tmp/nope" }] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);

    // Unknown transaction: TX_STALE.
    let err = clip
        .request(
            "transactions.fill",
            json!({ "tx_id": "tx_nope", "entries": [{ "file_id": "f0", "dest_path": "/tmp/nope" }] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");
}

#[tokio::test]
async fn fill_requires_the_read_scope() {
    let core = TestCore::start().await;
    let mut tray = spawn_component(&core, "tray", "tray", &["session.read"]).await;
    let err = tray
        .request(
            "transactions.fill",
            json!({ "tx_id": "tx_x", "entries": [{ "file_id": "f0", "dest_path": "/tmp/x" }] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

// ---------------------------------------------------------------------------
// Brick 2 (Android): materialized transactions (push-at-copy). The source ships
// the inline bytes at copy time; the destination caches them and serves its
// pastes locally, so the source may vanish right after copying.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_materialized_copy_is_served_from_the_destination_cache() {
    let server = TestServer::start().await;
    let (_a, mut ca, b, mut cb) = connected_pair(&server).await;

    // A copies with `materialize`: the bytes are pushed with the announce.
    let tx = announce_text_materialized(&mut ca, "pushed at copy").await;

    // B learns it — the note fires only once the pushed bytes are cached, and
    // the size is the exact decoded length.
    let note = cb.wait_notification("clipboard.remote_updated").await;
    assert_eq!(note["tx_id"], json!(tx));
    assert_eq!(note["formats"], json!([{ "format": "text", "size": 14 }]));

    // B pastes: the blob comes from B's OWN cache — no `clip_session` to A, and
    // A's backend is never asked for `clipboard.get_data`.
    let token = open_channel_token(&mut cb, &tx).await;
    let mut ch = b.open_channel(&token).await;
    assert_eq!(ch.fetch("text").await.unwrap(), b"pushed at copy");

    // The proof of "served locally": A's backend saw no request during the
    // paste (a pull-at-paste clip would have hit `clipboard.get_data` here).
    ca.assert_silent().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_materialized_paste_survives_the_source_going_offline() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = connected_pair(&server).await;

    let tx = announce_text_materialized(&mut ca, "outlives its source").await;
    // Ensure the push has landed and been cached BEFORE the source leaves.
    cb.wait_notification("clipboard.remote_updated").await;

    // The source (a phone the OS would kill) goes away entirely: its Core stops.
    drop(ca);
    drop(a);

    // B still pastes: `transactions.open` does NOT fail `DEVICE_OFFLINE` (a
    // materialized clip is exempt from the reachability check), and the FETCH is
    // served from the local cache with the source unreachable.
    let token = open_channel_token(&mut cb, &tx).await;
    let mut ch = b.open_channel(&token).await;
    assert_eq!(ch.fetch("text").await.unwrap(), b"outlives its source");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_materialized_clip_is_superseded_and_freed_like_any_other() {
    let server = TestServer::start().await;
    let (_a, mut ca, b, mut cb) = connected_pair(&server).await;

    let tx1 = announce_text_materialized(&mut ca, "first materialized").await;
    let n1 = cb.wait_notification("clipboard.remote_updated").await;
    assert_eq!(n1["tx_id"], json!(tx1));

    // A newer copy supersedes it globally; B converges onto tx2.
    let tx2 = announce_text_materialized(&mut ca, "second materialized").await;
    let n2 = cb.wait_notification("clipboard.remote_updated").await;
    assert_eq!(n2["tx_id"], json!(tx2));

    // The superseded materialized clip refuses a NEW session (its cache is gone
    // with it), while the fresh one still pastes from its cache.
    assert_eq!(
        cb.request("transactions.open", json!({ "tx_id": tx1 }))
            .await
            .unwrap_err()
            .app_code(),
        "TX_STALE"
    );
    let token = open_channel_token(&mut cb, &tx2).await;
    let mut ch = b.open_channel(&token).await;
    assert_eq!(ch.fetch("text").await.unwrap(), b"second materialized");
}

#[tokio::test(flavor = "multi_thread")]
async fn materialize_refuses_sensitive_and_files() {
    let server = TestServer::start().await;
    let (a, mut ca, _b, _cb) = connected_pair(&server).await;

    let b64 = base64::engine::general_purpose::STANDARD.encode(b"secret");
    // sensitive + materialize is a contradiction: a concealed clip stays
    // pull-at-paste.
    let err = ca
        .request(
            "clipboard.updated",
            json!({
                "formats": [{ "format": "text" }],
                "sensitive": true,
                "materialize": true,
                "blobs": { "text": b64 },
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);

    // files + materialize: a manifest is not bytes (files use pull / fill).
    let path = a.write_source("x.bin", b"x");
    let err = ca
        .request(
            "clipboard.updated",
            json!({
                "formats": [{ "format": "files" }],
                "paths": [path.to_string_lossy()],
                "materialize": true,
                "blobs": {},
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

// ---------------------------------------------------------------------------
// Raw attested peer: forges `clip_announce` frames a legitimate backend never
// would (a compromised device of the account), to exercise the receiver's
// convergence and fail-closed validation over the wire.
// ---------------------------------------------------------------------------

async fn open_channel_token(c: &mut TestComponent, tx_id: &str) -> String {
    c.request("transactions.open", json!({ "tx_id": tx_id }))
        .await
        .expect("transactions.open")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string()
}

/// A Core `b` with a subscribed backend, plus a raw attested peer of the same
/// account that `b` already sees (its stream will pass `peer_in_directory`).
async fn core_with_raw_peer(server: &TestServer) -> (TestCore, TestComponent, RawPeer) {
    let switchboard = MemorySwitchboard::new();
    let code = universallink_core::account_key::generate_recovery_code();
    let b = TestCore::start_enrolled_on_with_code(server, &switchboard, Some(&code)).await;
    let mut cb = backend(&b).await;
    subscribe(&mut cb).await;
    wait_server_connected(&mut cb, true).await;
    let raw = RawPeer::attested(server, &switchboard, &code).await;
    // B must see the raw peer attested, or its stream is refused fail-closed.
    wait_attested(&mut cb, &raw.device_id).await;
    (b, cb, raw)
}

struct RawPeer {
    device_id: String,
    transport: Arc<MemoryTransport>,
    _conn: TestConn,
}

impl RawPeer {
    async fn attested(server: &TestServer, switchboard: &MemorySwitchboard, code: &str) -> RawPeer {
        let key = DeviceKey::generate();
        let mut conn = server.connect_direct().await;
        let device_id = enroll_key(
            &mut conn,
            &server.oidc,
            &key,
            TEST_SUB,
            "raw-peer",
            std::env::consts::OS,
        )
        .await;
        authenticate(&mut conn, &key, &device_id).await;
        let ak = universallink_core::account_key::account_key_from_code(code).expect("valid code");
        let att = universallink_core::account_key::attest(&ak, &key.node_id());
        let relay = format!("iroh+memory://{}", key.node_id());
        conn.request(
            "presence.update",
            json!({ "attestation": att, "relay_url": relay }),
        )
        .await
        .expect("presence.update");
        let transport = switchboard.endpoint(key.node_id(), Some(relay));
        RawPeer {
            device_id,
            transport,
            _conn: conn,
        }
    }

    /// Accepts the next incoming stream (a `clip_session` the paster opens to
    /// us) and drops it without serving — a source that vanishes mid-session
    /// with no graceful `TX_STALE`, yielding `PEER_GONE` on the paster side.
    async fn abandon_next_session(&self) {
        if let Ok((_peer, mut stream)) = self.transport.accept().await {
            // Read a little (the opening frame) so the paster's session is truly
            // in flight, then drop the stream → the paster sees the reset.
            let mut scratch = [0u8; 64];
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut scratch)).await;
        }
    }

    /// Opens a data-plane stream to `target` and writes a `clip_announce` frame
    /// (u32-BE length + JSON), then reads the ack and closes — the source half
    /// of `clipnet::recv_announce`.
    async fn send_clip_announce(&self, target: &TestCore, mut announce: Value) {
        let peer = PeerAddr {
            node_id: target.key().node_id(),
            relay_url: Some(format!("iroh+memory://{}", target.key().node_id())),
        };
        let mut stream = self
            .transport
            .open(&peer)
            .await
            .expect("the switchboard routes");
        announce["type"] = json!("clip_announce");
        let bytes = serde_json::to_vec(&announce).unwrap();
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .expect("frame length");
        stream.write_all(&bytes).await.expect("frame body");
        stream.flush().await.expect("flush");
        // The receiver acks then closes; drain briefly so the announce is fully
        // processed before we drop the stream.
        let mut len = [0u8; 4];
        let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut len)).await;
    }
}
