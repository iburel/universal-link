// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Published transactions - the generic, long-lived producer (doc/core-api.md,
//! "Transactions"; ticket #83).
//!
//! `transactions.publish` freezes a manifest exactly like a clipboard announce
//! but OUTSIDE the election: the current clip is untouched, nothing supersedes
//! the published transaction, and it serves any number of sessions until
//! `transactions.revoke` (which cuts open channels NOW, the logout path), its
//! publishing connection closes, or the account's grants die with the session.
//! Consuming requires the scope of the transaction's PRODUCER: `clipboard.read`
//! for a clipboard transaction, `transactions.publish` for a published one.
//! `transactions.adopt` installs a transaction published on another device of
//! the account (fetching and fail-closed re-validating its frozen manifest), so
//! the untouched consumer machinery serves it exactly like a remote clip.

use serde_json::json;

use crate::clipboard_net::RawPeer;
use crate::support::*;

/// A producer the ticket's way: ANY role may publish - the scope alone guards
/// it (unlike announcing, which is the exclusive clipboard backend's).
async fn producer(core: &TestCore) -> TestComponent {
    spawn_component(core, "engine", "custom", &["transactions.publish"]).await
}

/// A producer with what the cross-Core tests also need: the directory (waits),
/// the session flag, and the transfer events (fills).
async fn net_producer(core: &TestCore) -> TestComponent {
    spawn_component(
        core,
        "engine",
        "custom",
        &[
            "transactions.publish",
            "devices.read",
            "session.read",
            "transfers.read",
        ],
    )
    .await
}

/// Publishes one on-disk file and returns `(tx_id, first file_id)`.
async fn publish_file(
    core: &TestCore,
    c: &mut TestComponent,
    name: &str,
    contents: &[u8],
) -> (String, String) {
    let path = core.write_source(name, contents);
    let r = c
        .request(
            "transactions.publish",
            json!({ "paths": [path.to_string_lossy()] }),
        )
        .await
        .expect("transactions.publish");
    let tx_id = r["tx_id"].as_str().expect("tx_id").to_string();
    let file_id = r["files"][0]["file_id"]
        .as_str()
        .expect("file_id")
        .to_string();
    (tx_id, file_id)
}

async fn open_token(c: &mut TestComponent, tx_id: &str) -> String {
    c.request("transactions.open", json!({ "tx_id": tx_id }))
        .await
        .expect("transactions.open")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string()
}

// ---------------------------------------------------------------------------
// The local producer.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn publish_requires_the_producer_scope_and_no_role() {
    let core = TestCore::start().await;

    // No scope: denied - and the role is irrelevant, unlike announcing.
    let mut bare = spawn_component(&core, "bare", "custom", &["clipboard.read"]).await;
    let err = bare
        .request("transactions.publish", json!({ "paths": ["/tmp/x"] }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");

    // The scope on a plain `custom` role publishes.
    let mut engine = producer(&core).await;
    let (tx_id, _) = publish_file(&core, &mut engine, "a.txt", b"published").await;
    assert!(tx_id.starts_with("tx_"), "unguessable tx_id: {tx_id}");
}

#[tokio::test]
async fn publish_returns_the_frozen_manifest_and_serves_reads() {
    let core = TestCore::start().await;
    let mut engine = producer(&core).await;

    let path = core.write_source("data.bin", b"0123456789");
    let r = engine
        .request(
            "transactions.publish",
            json!({ "paths": [path.to_string_lossy()] }),
        )
        .await
        .unwrap();
    // The publisher learns the manifest it will speak to its peers about -
    // relative wire paths and Core-assigned file ids, never the source paths.
    assert_eq!(r["files"][0]["path"], json!("data.bin"));
    assert_eq!(r["files"][0]["size"], json!(10));
    let tx_id = r["tx_id"].as_str().unwrap().to_string();
    let file_id = r["files"][0]["file_id"].as_str().unwrap().to_string();

    // Consume through the untouched machinery: open, attach, range reads.
    let token = open_token(&mut engine, &tx_id).await;
    let mut ch = core.open_channel(&token).await;
    assert_eq!(ch.read(&file_id, 0, 10).await.unwrap(), b"0123456789");
    assert_eq!(ch.read(&file_id, 4, 3).await.unwrap(), b"456");

    // Long-lived: a second session opens as freely as the first (no
    // supersession sweeps it).
    let token = open_token(&mut engine, &tx_id).await;
    let mut ch2 = core.open_channel(&token).await;
    assert_eq!(ch2.read(&file_id, 0, 2).await.unwrap(), b"01");
}

#[tokio::test]
async fn publish_stays_outside_the_clipboard_election() {
    let core = TestCore::start().await;
    let mut clip = spawn_component(
        &core,
        "clipboard",
        "clipboard-backend",
        &["clipboard.read", "clipboard.write"],
    )
    .await;
    let mut engine = producer(&core).await;

    // A clip, then a publish: the current clip must still be the clip.
    let announced = clip
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "text" }] }),
        )
        .await
        .unwrap()["tx_id"]
        .clone();
    let (published, _) = publish_file(&core, &mut engine, "p.txt", b"x").await;
    let current = clip.request("clipboard.current", json!({})).await.unwrap();
    assert_eq!(current["tx_id"], announced);

    // And the next announce supersedes the CLIP, never the published one.
    clip.request(
        "clipboard.updated",
        json!({ "formats": [{ "format": "image/png" }] }),
    )
    .await
    .unwrap();
    let token = engine
        .request("transactions.open", json!({ "tx_id": published }))
        .await
        .expect("published transaction still openable")["channel_token"]
        .clone();
    assert!(token.as_str().is_some());
}

#[tokio::test]
async fn consuming_requires_the_scope_of_the_producer() {
    let core = TestCore::start().await;
    let mut clip = spawn_component(
        &core,
        "clipboard",
        "clipboard-backend",
        &["clipboard.read", "clipboard.write"],
    )
    .await;
    let mut engine = producer(&core).await;

    let clip_tx = clip
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "text" }] }),
        )
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (pub_tx, pub_file) = publish_file(&core, &mut engine, "p.txt", b"pp").await;

    // The producer scope does not open a CLIPBOARD transaction…
    let err = engine
        .request("transactions.open", json!({ "tx_id": clip_tx }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
    // …and `clipboard.read` does not open a PUBLISHED one, in either verb.
    let err = clip
        .request("transactions.open", json!({ "tx_id": pub_tx }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
    let err = clip
        .request(
            "transactions.fill",
            json!({ "tx_id": pub_tx, "entries": [
                { "file_id": pub_file, "dest_path": core.config_dir().join("out").to_string_lossy() },
            ] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test]
async fn revoke_cuts_open_sessions_now() {
    let core = TestCore::start().await;
    let mut engine = producer(&core).await;
    let (tx_id, file_id) = publish_file(&core, &mut engine, "cut.txt", b"cut me").await;

    let token = open_token(&mut engine, &tx_id).await;
    let mut ch = core.open_channel(&token).await;
    assert_eq!(ch.read(&file_id, 0, 6).await.unwrap(), b"cut me");

    engine
        .request("transactions.revoke", json!({ "tx_id": tx_id }))
        .await
        .expect("transactions.revoke");
    // The waiting channel is cut with TX_STALE - revoking means NOW, the very
    // path a logout takes.
    assert_eq!(ch.next_response().await.unwrap_err(), "TX_STALE");
    // No new session either.
    let err = engine
        .request("transactions.open", json!({ "tx_id": tx_id }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");
    // Idempotent: a revoke retried after the fact is not an error.
    engine
        .request("transactions.revoke", json!({ "tx_id": tx_id }))
        .await
        .expect("idempotent revoke");
}

#[tokio::test]
async fn revoke_spares_other_transactions_and_refuses_clipboard_ones() {
    let core = TestCore::start().await;
    let mut clip = spawn_component(
        &core,
        "clipboard",
        "clipboard-backend",
        &["clipboard.read", "clipboard.write"],
    )
    .await;
    let mut engine = producer(&core).await;

    let (keep_tx, keep_file) = publish_file(&core, &mut engine, "keep.txt", b"keep").await;
    let (drop_tx, _) = publish_file(&core, &mut engine, "drop.txt", b"drop").await;
    let keep_token = open_token(&mut engine, &keep_tx).await;
    let mut keep_ch = core.open_channel(&keep_token).await;
    assert_eq!(keep_ch.read(&keep_file, 0, 4).await.unwrap(), b"keep");

    // Revoking one published transaction wakes every session; only the revoked
    // one's are cut - the survivor keeps serving.
    engine
        .request("transactions.revoke", json!({ "tx_id": drop_tx }))
        .await
        .unwrap();
    assert_eq!(keep_ch.read(&keep_file, 0, 4).await.unwrap(), b"keep");

    // A clipboard transaction is not the producer's to revoke: its lifecycle
    // belongs to the election.
    let clip_tx = clip
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "text" }] }),
        )
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    let err = engine
        .request("transactions.revoke", json!({ "tx_id": clip_tx }))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
    // The clip is untouched.
    let current = clip.request("clipboard.current", json!({})).await.unwrap();
    assert_eq!(current["tx_id"], json!(clip_tx));
}

#[tokio::test]
async fn published_transactions_die_with_their_publisher() {
    let core = TestCore::start().await;
    let mut engine = producer(&core).await;
    let (tx_id, file_id) = publish_file(&core, &mut engine, "own.txt", b"owned").await;

    // Another holder of the scope consumes it (possession of the tx_id is the
    // authorization).
    let mut other = spawn_component(&core, "other", "custom", &["transactions.publish"]).await;
    let token = open_token(&mut other, &tx_id).await;
    let mut ch = core.open_channel(&token).await;
    assert_eq!(ch.read(&file_id, 0, 5).await.unwrap(), b"owned");

    // The publisher disconnects: the grant dies with its holder - the open
    // session is cut and no new one opens.
    drop(engine);
    assert_eq!(ch.next_response().await.unwrap_err(), "TX_STALE");
    let err = other
        .request("transactions.open", json!({ "tx_id": tx_id }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");
}

#[tokio::test]
async fn logout_drops_published_transactions_too() {
    let server = TestServer::start().await;
    let core = TestCore::start_enrolled(&server).await;
    let mut engine = producer(&core).await;
    let (tx_id, file_id) = publish_file(&core, &mut engine, "s.txt", b"secret").await;
    let token = open_token(&mut engine, &tx_id).await;
    let mut ch = core.open_channel(&token).await;
    assert_eq!(ch.read(&file_id, 0, 6).await.unwrap(), b"secret");

    // The account's read grants do not outlive its session - published or not.
    let mut mgr = spawn_component(&core, "mgr", "custom", &["session.manage"]).await;
    mgr.request("session.logout", json!({})).await.unwrap();

    assert_eq!(ch.read(&file_id, 0, 6).await.unwrap_err(), "TX_STALE");
    let err = engine
        .request("transactions.open", json!({ "tx_id": tx_id }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");
}

#[tokio::test]
async fn publish_validates_its_paths() {
    let core = TestCore::start().await;
    let mut engine = producer(&core).await;

    // An empty list publishes nothing.
    let err = engine
        .request("transactions.publish", json!({ "paths": [] }))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
    // A missing path refuses the WHOLE publish, like a clipboard announce.
    let ghost = core.config_dir().join("no-such-file");
    let err = engine
        .request(
            "transactions.publish",
            json!({ "paths": [ghost.to_string_lossy()] }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn a_published_file_that_changed_refuses_the_read() {
    let core = TestCore::start().await;
    let mut engine = producer(&core).await;
    let path = core.write_source("frozen.txt", b"before");
    let r = engine
        .request(
            "transactions.publish",
            json!({ "paths": [path.to_string_lossy()] }),
        )
        .await
        .unwrap();
    let tx_id = r["tx_id"].as_str().unwrap().to_string();
    let file_id = r["files"][0]["file_id"].as_str().unwrap().to_string();

    // The frozen file is rewritten under our feet: the read is refused rather
    // than serving different bytes.
    std::fs::write(&path, b"rewritten-and-longer").unwrap();
    let token = open_token(&mut engine, &tx_id).await;
    let mut ch = core.open_channel(&token).await;
    assert_eq!(ch.read(&file_id, 0, 6).await.unwrap_err(), "FILE_CHANGED");
}

#[tokio::test]
async fn fill_from_a_published_transaction() {
    let core = TestCore::start().await;
    let mut engine = spawn_component(
        &core,
        "engine",
        "custom",
        &["transactions.publish", "transfers.read"],
    )
    .await;
    engine
        .request("events.subscribe", json!({ "topics": ["transfers"] }))
        .await
        .unwrap();
    let (tx_id, file_id) = publish_file(&core, &mut engine, "src.bin", b"fill me up").await;

    let dest = core.config_dir().join("landed.bin");
    engine
        .request(
            "transactions.fill",
            json!({ "tx_id": tx_id, "entries": [
                { "file_id": file_id, "dest_path": dest.to_string_lossy() },
            ] }),
        )
        .await
        .expect("transactions.fill");
    engine.wait_notification("transfer.finished").await;
    assert_eq!(std::fs::read(&dest).unwrap(), b"fill me up");
}

// ---------------------------------------------------------------------------
// Adoption: the published transaction across devices.
// ---------------------------------------------------------------------------

/// A connected pair with a producer component on each side.
async fn adopted_pair(server: &TestServer) -> (TestCore, TestComponent, TestCore, TestComponent) {
    let (a, b) = TestCore::start_pair(server).await;
    let mut ea = net_producer(&a).await;
    let mut eb = net_producer(&b).await;
    wait_server_connected(&mut ea, true).await;
    wait_server_connected(&mut eb, true).await;
    wait_reachable(&mut ea, b.device_id()).await;
    wait_reachable(&mut eb, a.device_id()).await;
    (a, ea, b, eb)
}

#[tokio::test(flavor = "multi_thread")]
async fn adopt_installs_and_serves_a_remote_published_transaction() {
    let server = TestServer::start().await;
    let (a, mut ea, b, mut eb) = adopted_pair(&server).await;

    let (tx_id, file_id) = publish_file(&a, &mut ea, "shared.bin", b"across the wire").await;

    // B installs the source's frozen manifest, re-validated fail-closed.
    let record = eb
        .request(
            "transactions.adopt",
            json!({ "device_id": a.device_id(), "tx_id": tx_id }),
        )
        .await
        .expect("transactions.adopt");
    assert_eq!(record["tx_id"], json!(tx_id));
    assert_eq!(record["files"][0]["path"], json!("shared.bin"));
    assert_eq!(record["files"][0]["file_id"], json!(file_id));

    // Adopting twice is a cheap idempotent re-install (a reconnected engine).
    let again = eb
        .request(
            "transactions.adopt",
            json!({ "device_id": a.device_id(), "tx_id": tx_id }),
        )
        .await
        .expect("idempotent adopt");
    assert_eq!(again["files"], record["files"]);

    // The untouched consumer machinery serves it: ranges over the wire…
    let token = open_token(&mut eb, &tx_id).await;
    let mut ch = b.open_channel(&token).await;
    assert_eq!(ch.read(&file_id, 7, 8).await.unwrap(), b"the wire");
    drop(ch);

    // …and a fill that lands on B's disk.
    eb.request("events.subscribe", json!({ "topics": ["transfers"] }))
        .await
        .unwrap();
    let dest = b.config_dir().join("adopted.bin");
    eb.request(
        "transactions.fill",
        json!({ "tx_id": tx_id, "entries": [
            { "file_id": file_id, "dest_path": dest.to_string_lossy() },
        ] }),
    )
    .await
    .expect("transactions.fill");
    eb.wait_notification("transfer.finished").await;
    assert_eq!(std::fs::read(&dest).unwrap(), b"across the wire");
}

#[tokio::test(flavor = "multi_thread")]
async fn adopt_refuses_unknown_ids_clipboard_ids_and_unknown_devices() {
    let server = TestServer::start().await;
    let (a, _ea, _b, mut eb) = adopted_pair(&server).await;

    // The source does not back that id: nothing installs.
    let err = eb
        .request(
            "transactions.adopt",
            json!({ "device_id": a.device_id(), "tx_id": "tx_deadbeef" }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");

    // A clipboard transaction is not adoptable - same refusal, disclosing
    // nothing (its metadata travels by `clip_announce`).
    let mut clip = spawn_component(
        &a,
        "clipboard",
        "clipboard-backend",
        &["clipboard.read", "clipboard.write"],
    )
    .await;
    let clip_tx = clip
        .request(
            "clipboard.updated",
            json!({ "formats": [{ "format": "text" }] }),
        )
        .await
        .unwrap()["tx_id"]
        .as_str()
        .unwrap()
        .to_string();
    let err = eb
        .request(
            "transactions.adopt",
            json!({ "device_id": a.device_id(), "tx_id": clip_tx }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");

    // An unknown device: fail-closed, indistinguishable from unattested.
    let err = eb
        .request(
            "transactions.adopt",
            json!({ "device_id": "d_nobody", "tx_id": "tx_x" }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");

    // And the scope guards the door.
    let mut bare = spawn_component(&_b, "bare", "custom", &["devices.read"]).await;
    let err = bare
        .request(
            "transactions.adopt",
            json!({ "device_id": a.device_id(), "tx_id": "tx_x" }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_source_revoke_cuts_remote_consumers_and_evicts_the_adoption() {
    let server = TestServer::start().await;
    let (a, mut ea, b, mut eb) = adopted_pair(&server).await;

    let (tx_id, file_id) = publish_file(&a, &mut ea, "gone.bin", b"going going").await;
    eb.request(
        "transactions.adopt",
        json!({ "device_id": a.device_id(), "tx_id": tx_id }),
    )
    .await
    .unwrap();
    let token = open_token(&mut eb, &tx_id).await;
    let mut ch = b.open_channel(&token).await;
    assert_eq!(ch.read(&file_id, 0, 5).await.unwrap(), b"going");

    // The source revokes: the relayed TX_STALE cuts B's open session…
    ea.request("transactions.revoke", json!({ "tx_id": tx_id }))
        .await
        .unwrap();
    assert_eq!(ch.next_response().await.unwrap_err(), "TX_STALE");

    // …and evicts B's adopted entry: a later open refuses at the control
    // plane, no dial to a promise nobody backs. (The eviction races the cut by
    // a hair, hence the wait.)
    eventually(
        async || {
            eb.request("transactions.open", json!({ "tx_id": &tx_id }))
                .await
                .err()
                .is_some_and(|e| e.app_code() == "TX_STALE")
        },
        "the adopted entry is evicted after the source's revoke",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hostile_manifest_installs_nothing() {
    let server = TestServer::start().await;
    let switchboard = onedevice_test_support::memory_transport::MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let b = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let mut eb = net_producer(&b).await;
    wait_server_connected(&mut eb, true).await;
    let raw = RawPeer::attested(&server, &switchboard, &code).await;
    wait_attested(&mut eb, &raw.device_id).await;

    // The "source" answers the fetch with a traversal manifest: the fail-closed
    // re-validation drops it whole - TX_STALE, nothing installed.
    let adopt = eb.request(
        "transactions.adopt",
        json!({ "device_id": raw.device_id, "tx_id": "tx_evil" }),
    );
    let serve = async {
        let mut stream = raw
            .next_stream_of("tx_fetch")
            .await
            .expect("the adopt dials");
        let reply = json!({
            "type": "tx_manifest",
            "tx_id": "tx_evil",
            "formats": [],
            "files": [ { "file_id": "f0", "path": "../escape", "size": 4 } ],
        });
        let bytes = serde_json::to_vec(&reply).unwrap();
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(&bytes).await.unwrap();
        stream.flush().await.unwrap();
        // Hold the reply until the initiator closes (QUIC lifecycle).
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::io::AsyncReadExt::read(&mut stream, &mut [0u8; 16]),
        )
        .await;
    };
    let (adopted, ()) = tokio::join!(adopt, serve);
    assert_eq!(adopted.unwrap_err().app_code(), "TX_STALE");

    // Nothing installed: opening that id refuses locally.
    let err = eb
        .request("transactions.open", json!({ "tx_id": "tx_evil" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "TX_STALE");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_adopted_entry_dies_with_its_adopter() {
    let server = TestServer::start().await;
    let (a, mut ea, b, mut eb) = adopted_pair(&server).await;

    let (tx_id, _) = publish_file(&a, &mut ea, "mine.bin", b"mine").await;
    eb.request(
        "transactions.adopt",
        json!({ "device_id": a.device_id(), "tx_id": tx_id }),
    )
    .await
    .unwrap();
    drop(eb);

    // A fresh component must re-adopt: the previous adoption died with its
    // holder. The re-adopt then works - the source still backs the id.
    let mut fresh = net_producer(&b).await;
    eventually(
        async || {
            fresh
                .request("transactions.open", json!({ "tx_id": &tx_id }))
                .await
                .err()
                .is_some_and(|e| e.app_code() == "TX_STALE")
        },
        "the adopted entry dies with the adopting connection",
    )
    .await;
    fresh
        .request(
            "transactions.adopt",
            json!({ "device_id": a.device_id(), "tx_id": tx_id }),
        )
        .await
        .expect("re-adopting after a reconnect");
}
