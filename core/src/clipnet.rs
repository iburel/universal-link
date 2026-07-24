// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The clipboard network plane: propagation of copies between the account's
//! Cores, and the byte relay behind a remote paste (doc/core-api.md,
//! "Transactions", "The data channel" — network mapping).
//!
//! Two peer protocols, both over the data plane (`dataplane`), each a framed
//! JSON control frame (`type`) on a fresh bidirectional stream — dispatched by
//! `dataplane::serve_incoming` exactly like the file-transfer `offer`:
//!
//! - **`clip_announce`** (source → every online peer): the metadata of a local
//!   copy. The receiver re-validates the manifest fail-closed, applies the
//!   global last-copier-wins (`(seq, device_id)`), stores a REMOTE transaction,
//!   and pushes `clipboard.remote_updated` to its local backend. Best-effort:
//!   an offline peer simply re-learns on the next copy.
//! - **`clip_session`** (destination → source, one per paste session): carries
//!   the very data-channel binary protocol (`datachannel`). The source runs the
//!   unchanged `serve_consumer` over it (disk ranges + inline pulls from its own
//!   backend), so a remote paste is byte-identical to a local one — and the
//!   open stream counts as a session on the source, so copying something else
//!   there never cuts an in-flight remote paste (supersession survives across
//!   Cores). The destination either transparently pipes it to a local consumer
//!   channel (`pipe_consumer`) or drives it itself to fill files
//!   (`transactions.fill`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

use crate::clipboard::{FillPlan, Origin, ServeMode, Transaction};
use crate::connector::IoStream;
use crate::datachannel;
use crate::dataplane::{self, PeerAddr};
use crate::state::AppState;

/// Budget for opening a stream to the source (resolution + iroh handshake).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the announcer waits for the receiver's ack before giving up (the
/// receiver is otherwise silent — a best-effort delivery).
const ANNOUNCE_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a receiver holds the announce stream after its ack, so the ack is
/// not abandoned in flight on the QUIC side (as `dataplane::write_ack` does).
const LINGER: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Brick 4: propagation of a local copy.
// ---------------------------------------------------------------------------

/// Broadcasts a local copy to the account's other online devices, fire-and-
/// forget: each learns the new clip and supersedes what it had. Best-effort —
/// a peer that is offline, unreachable, or slow is skipped (convergence catches
/// up on the next copy). Does nothing when not logged in (no peers).
pub(crate) fn propagate(state: &Arc<AppState>, announce: Value) {
    let peers = dataplane::account_peers(state);
    if peers.is_empty() {
        return;
    }
    // A manifest too large for a single data-plane frame cannot propagate (a v1
    // limit — lazy enumeration will lift it). Detect it ONCE here, rather than
    // failing identically against every peer: the clip stays local, and the
    // reason is visible instead of silent. Headroom for the added `type` field.
    let serialized = serde_json::to_vec(&announce).map_or(usize::MAX, |b| b.len());
    if serialized + 64 > dataplane::MAX_FRAME as usize {
        tracing::warn!(
            entries = serialized,
            "clipboard clip too large to propagate to peers; it stays local (v1 limit)"
        );
        return;
    }
    for peer in peers {
        let state = state.clone();
        let announce = announce.clone();
        tokio::spawn(async move {
            if let Err(e) = send_announce(&state, &peer, &announce).await {
                tracing::debug!(peer = %peer.node_id, error = %e, "clipboard announce not delivered");
            }
        });
    }
}

async fn send_announce(
    state: &Arc<AppState>,
    peer: &PeerAddr,
    announce: &Value,
) -> std::io::Result<()> {
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, state.transport.open(peer))
        .await
        .map_err(|_| timed_out("connect"))??;
    let mut frame = announce.clone();
    frame["type"] = json!("clip_announce");
    dataplane::write_frame(&mut stream, &serde_json::to_vec(&frame)?).await?;
    // Wait for the receiver's ack, then close — the close tells the receiver the
    // ack arrived (it drains until then). A missed ack is not fatal: the copy is
    // best-effort, so we close anyway.
    let _ = tokio::time::timeout(ANNOUNCE_ACK_TIMEOUT, dataplane::read_frame(&mut stream)).await;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Receiver side of a `clip_announce`: re-validates the announce, adopts it if
/// it wins the global election, notifies the local backend, and acks.
pub(crate) async fn recv_announce(
    state: Arc<AppState>,
    peer_node_id: String,
    first: Value,
    mut stream: Box<dyn IoStream>,
) {
    if let Some(tx) = build_remote_tx(&state, &peer_node_id, &first) {
        // Compute the record before the move; notify only if the announce is
        // adopted as the new current clip (the global last-copier-wins).
        let record = tx.record();
        let adopted = state
            .clipboard
            .lock()
            .expect("lock clipboard")
            .announce_remote(tx)
            .is_some();
        if adopted {
            state.registry.lock().expect("lock registry").notify_topic(
                "clipboard",
                "clipboard.remote_updated",
                &record,
            );
        }
    }
    // Ack + linger (QUIC lifecycle): let the source read the ack before it
    // closes, then observe its close.
    let ack = serde_json::to_vec(&json!({ "type": "clip_ack" })).expect("serialize ack");
    let _ = dataplane::write_frame(&mut stream, &ack).await;
    let _ = stream.shutdown().await;
    let _ = tokio::time::timeout(LINGER, dataplane::drain(&mut stream)).await;
}

/// Builds a REMOTE transaction from a validated `clip_announce`, or `None` to
/// drop it fail-closed. Binds the announce to the authenticated peer (the
/// claimed `device_id` must resolve, in our directory, to the very `node_id`
/// iroh authenticated) and re-validates the manifest.
fn build_remote_tx(state: &AppState, peer_node_id: &str, first: &Value) -> Option<Transaction> {
    let tx_id = first.get("tx_id").and_then(Value::as_str)?.to_string();
    let device_id = first.get("device_id").and_then(Value::as_str)?.to_string();
    let seq = first.get("seq").and_then(Value::as_u64)?;
    let sensitive = match first.get("sensitive") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return None,
    };
    // The source claims to be `device_id`; it must be the peer iroh
    // authenticated — a device cannot announce a clip in another's name.
    let resolved = dataplane::resolve_peer(state, &device_id)?;
    if resolved.node_id != peer_node_id {
        return None;
    }
    let formats = crate::clipboard::parse_formats(first).ok()?;
    let files = match first.get("files") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => crate::clipboard::validate_remote_manifest(v.as_array()?)?,
    };
    // A `files` format iff a non-empty manifest — no silent mismatch, as on the
    // source side (`clipboard.updated`).
    let has_files = formats.iter().any(|f| f.format == "files");
    if has_files != !files.is_empty() {
        return None;
    }
    Some(Transaction {
        tx_id,
        device_id: Some(device_id.clone()),
        seq,
        formats,
        files,
        sensitive,
        origin: Origin::Remote {
            node_id: peer_node_id.to_string(),
            device_id,
        },
        superseded: false,
        sessions: 0,
        // A `clip_announce` never carries bytes; a materialized clip's cache is
        // filled from the trailing `clip_push` blobs (`recv_push`), not here.
        materialized: HashMap::new(),
    })
}

// ---------------------------------------------------------------------------
// Materialized transactions (push-at-copy): the source ships the inline bytes
// to every online device at copy time, so an ephemeral source (a phone) may
// then vanish (doc/core-api.md — "Materialized transactions"). Each device
// caches them and serves its pastes locally, never opening a `clip_session`.
// ---------------------------------------------------------------------------

/// Chunk size for streaming a materialized blob over `clip_push` — under the
/// data channel's `MAX_MSG`, so each chunk is one message the receiver reads.
const PUSH_CHUNK: usize = 64 * 1024;

/// Broadcasts a materialized copy: a `clip_push` to every online device,
/// carrying the announce metadata then the inline blobs. Fire-and-forget like
/// `propagate` (best-effort — an offline device simply misses the clip; it
/// re-learns nothing, exactly as a missed announce). `blobs` is the per-format
/// bytes; sharing them across the per-peer tasks is a cheap `Arc` clone.
pub(crate) fn propagate_materialized(
    state: &Arc<AppState>,
    announce: Value,
    blobs: crate::clipboard::MaterializedBlobs,
) {
    let peers = dataplane::account_peers(state);
    if peers.is_empty() {
        return;
    }
    // Only the METADATA frame is bounded by `MAX_FRAME`; the blobs stream
    // separately (capped by `MATERIALIZE_MAX`). An inline announce is tiny, so
    // this never fires in practice — kept for parity with `propagate`.
    let serialized = serde_json::to_vec(&announce).map_or(usize::MAX, |b| b.len());
    if serialized + 64 > dataplane::MAX_FRAME as usize {
        tracing::warn!("clipboard materialized metadata too large to propagate; it stays local");
        return;
    }
    let blobs = Arc::new(blobs);
    for peer in peers {
        let state = state.clone();
        let announce = announce.clone();
        let blobs = blobs.clone();
        tokio::spawn(async move {
            if let Err(e) = send_push(&state, &peer, &announce, &blobs).await {
                tracing::debug!(peer = %peer.node_id, error = %e, "materialized clip not pushed");
            }
        });
    }
}

/// Source half of a `clip_push`: opens a stream, writes the announce frame
/// (`type: clip_push`), streams each inline format's bytes as `DATA*`+`EOF` in
/// `formats` order, then waits for the receiver's ack and closes. The receiver
/// knows each blob's length from `formats[].size` (made exact at the announce),
/// so no per-blob header is needed.
async fn send_push(
    state: &Arc<AppState>,
    peer: &PeerAddr,
    announce: &Value,
    blobs: &[(String, Arc<Vec<u8>>)],
) -> std::io::Result<()> {
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, state.transport.open(peer))
        .await
        .map_err(|_| timed_out("connect"))??;
    let mut frame = announce.clone();
    frame["type"] = json!("clip_push");
    dataplane::write_frame(&mut stream, &serde_json::to_vec(&frame)?).await?;
    if let Some(formats) = announce.get("formats").and_then(Value::as_array) {
        for f in formats {
            let Some(fmt) = f.get("format").and_then(Value::as_str) else {
                continue;
            };
            if fmt == "files" {
                continue; // never materialized
            }
            let Some((_, bytes)) = blobs.iter().find(|(k, _)| k == fmt) else {
                // A format with no blob would desync the receiver's per-format
                // reads: abandon rather than send a truncated stream.
                return Err(datachannel::unexpected("materialize: missing blob"));
            };
            let mut offset = 0u64;
            for chunk in bytes.chunks(PUSH_CHUNK) {
                datachannel::write_data(&mut stream, offset, chunk).await?;
                offset += chunk.len() as u64;
            }
            datachannel::write_msg(&mut stream, datachannel::TAG_EOF, &[]).await?;
        }
    }
    let _ = tokio::time::timeout(ANNOUNCE_ACK_TIMEOUT, dataplane::read_frame(&mut stream)).await;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Receiver half of a `clip_push`: re-validates the announce, reads the trailing
/// inline blobs into the transaction's cache, adopts it if it wins the global
/// election, notifies the local backend — then acks. Mirrors `recv_announce`,
/// but the adopted transaction carries its bytes (a paste is served locally,
/// even after the source goes offline).
pub(crate) async fn recv_push(
    state: Arc<AppState>,
    peer_node_id: String,
    first: Value,
    mut stream: Box<dyn IoStream>,
) {
    if let Some((tx, record)) = build_pushed_tx(&state, &peer_node_id, &first, &mut stream).await {
        let adopted = state
            .clipboard
            .lock()
            .expect("lock clipboard")
            .announce_remote(tx)
            .is_some();
        if adopted {
            state.registry.lock().expect("lock registry").notify_topic(
                "clipboard",
                "clipboard.remote_updated",
                &record,
            );
        }
    }
    // Ack + linger, exactly as `recv_announce`: let the source read the ack
    // before it closes, then observe its close (also draining any blob bytes a
    // dropped push left unread).
    let ack = serde_json::to_vec(&json!({ "type": "clip_ack" })).expect("serialize ack");
    let _ = dataplane::write_frame(&mut stream, &ack).await;
    let _ = stream.shutdown().await;
    let _ = tokio::time::timeout(LINGER, dataplane::drain(&mut stream)).await;
}

/// Builds a materialized REMOTE transaction: the same fail-closed validation as
/// `build_remote_tx`, plus the inline-only / non-`sensitive` guard and reading
/// each format's blob off the stream into the cache. Returns the transaction and
/// its backend record, or `None` (drop, fail-closed) on any violation —
/// including a blob that over/under-runs its announced size.
async fn build_pushed_tx(
    state: &AppState,
    peer_node_id: &str,
    first: &Value,
    stream: &mut Box<dyn IoStream>,
) -> Option<(Transaction, Value)> {
    // A push MUST be flagged materialized (a plain announce carries no blobs),
    // inline-only, and never sensitive — a concealed clip stays pull-at-paste.
    if first.get("materialized").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let mut tx = build_remote_tx(state, peer_node_id, first)?;
    if tx.sensitive || !tx.files.is_empty() {
        return None;
    }
    let mut total = 0usize;
    for f in &tx.formats {
        // Every materialized format announces its exact length; the push carries
        // precisely that many bytes, and the running total is capped.
        let size = f.size? as usize;
        total = total.saturating_add(size);
        if total > crate::clipboard::MATERIALIZE_MAX {
            return None;
        }
        let bytes = read_blob(stream, size).await?;
        tx.materialized.insert(f.format.clone(), Arc::new(bytes));
    }
    let record = tx.record();
    Some((tx, record))
}

/// Reads one inline blob off a `clip_push` stream: `DATA*` then `EOF`, exactly
/// `expected` bytes. `None` on any framing error, an `ERROR` frame, a premature
/// `EOF`, an overrun, or a size mismatch — a truncated clip is never cached.
async fn read_blob(stream: &mut Box<dyn IoStream>, expected: usize) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(expected.min(PUSH_CHUNK * 2));
    loop {
        match datachannel::bounded(datachannel::read_msg(stream)).await {
            // A `DATA` frame MUST carry the 8-byte offset AND at least one byte
            // of data: a data-less frame is a protocol violation, never emitted
            // by a real push (`chunks()` yields no empty chunk, and an empty
            // blob is a bare `EOF`). Requiring progress here is also what keeps
            // this loop finite — every accepted frame advances `buf` toward the
            // overrun cap, so a peer cannot pin it with an endless drip of
            // zero-data frames (the per-frame stall budget alone would not).
            Ok(Some((datachannel::TAG_DATA, payload))) if payload.len() > 8 => {
                buf.extend_from_slice(&payload[8..]);
                if buf.len() > expected {
                    return None; // overruns the announced size
                }
            }
            Ok(Some((datachannel::TAG_EOF, _))) => break,
            _ => return None, // ERROR, premature/data-less frame, stall, or bad frame
        }
    }
    (buf.len() == expected).then_some(buf)
}

// ---------------------------------------------------------------------------
// Brick 5: the byte relay.
// ---------------------------------------------------------------------------

/// Source side of a `clip_session`: serve the paste from this device. The
/// transaction is LOCAL here, so `serve_consumer` reads its ranges from the disk
/// and pulls its inline blobs from the announcing backend — exactly as for a
/// local consumer channel.
pub(crate) async fn serve_session(state: Arc<AppState>, first: Value, stream: Box<dyn IoStream>) {
    let Some(tx_id) = first
        .get("tx_id")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let (reader, write) = tokio::io::split(stream);
    datachannel::serve_consumer(&state, reader, write, tx_id).await;
}

/// Destination side of a remote paste: opens a `clip_session` to the source and
/// transparently relays the data-channel binary protocol between the local
/// consumer and the source. The two directions run as independent copy loops so
/// two direction loops, each owning its own `read_msg` (never interleaved on one
/// task — `read_msg` is not cancel-safe). The DOWNSTREAM loop is the sole writer
/// of terminal errors to the consumer: it reads the source CONTINUOUSLY, so a
/// frame the source pushes on its own (a `TX_STALE` when the source stops/logs
/// out) is caught even between the consumer's requests; a source that vanishes
/// with no terminal frame surfaces as `PEER_GONE`; a reset of THIS Core cuts
/// with `TX_STALE`. The UPSTREAM loop forwards the consumer's requests; when it
/// ends (consumer gone, or a broken send) it shuts the write half so the source
/// — and hence downstream — unblocks, and we drive downstream to completion so
/// it always gets the last word.
pub(crate) async fn pipe_consumer<R, W>(
    state: &Arc<AppState>,
    mut consumer_read: R,
    mut consumer_write: W,
    tx_id: &str,
    node_id: &str,
    device_id: &str,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Resolve the source (C7 attestation) and open the session stream.
    let peer = match dataplane::resolve_peer(state, device_id) {
        // A re-enrolled source (new node_id) or one without a published relay is
        // no longer the device that made this offer: unreachable.
        Some(p) if p.node_id == node_id && p.relay_url.is_some() => p,
        _ => {
            let _ = datachannel::write_error(&mut consumer_write, "PEER_GONE").await;
            return;
        }
    };
    let net = tokio::time::timeout(CONNECT_TIMEOUT, state.transport.open(&peer)).await;
    let mut net = match net {
        Ok(Ok(s)) => s,
        _ => {
            let _ = datachannel::write_error(&mut consumer_write, "PEER_GONE").await;
            return;
        }
    };
    let frame = serde_json::to_vec(&json!({ "type": "clip_session", "tx_id": tx_id }))
        .expect("serialize clip_session");
    if dataplane::write_frame(&mut net, &frame).await.is_err() {
        let _ = datachannel::write_error(&mut consumer_write, "PEER_GONE").await;
        return;
    }
    let (mut net_read, mut net_write) = tokio::io::split(net);

    // Set by `up` when it ends because the CONSUMER left (closed or stalled), so
    // `down` — which then sees the write half shut — ends the session silently
    // (as a local paste does) rather than misreporting `PEER_GONE`. A genuine
    // source failure leaves it false, and `down` reports `PEER_GONE`. Worst-case
    // visibility race only degrades to the (still-correct) `PEER_GONE`.
    let consumer_gone = std::sync::atomic::AtomicBool::new(false);

    // Upstream: consumer requests → the source. On exit (consumer left, or a
    // broken send to a gone source) shut the write half so the source ends and
    // downstream's read unblocks.
    let up = async {
        let left = loop {
            match datachannel::bounded(datachannel::read_msg(&mut consumer_read)).await {
                Ok(Some((tag, payload))) => {
                    if datachannel::bounded(datachannel::write_msg(&mut net_write, tag, &payload))
                        .await
                        .is_err()
                    {
                        break false; // source gone
                    }
                }
                _ => break true, // consumer closed or stalled
            }
        };
        if left {
            consumer_gone.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let _ = net_write.shutdown().await;
    };

    // Downstream: the source's frames → the consumer, plus the terminal
    // conditions. The sole writer of consumer-facing errors.
    let down = async {
        let reset = state.clipboard_reset.notified();
        tokio::pin!(reset);
        loop {
            let msg = tokio::select! {
                biased;
                _ = &mut reset => {
                    let _ = datachannel::write_error(&mut consumer_write, "TX_STALE").await;
                    return;
                }
                m = datachannel::bounded(datachannel::read_msg(&mut net_read)) => m,
            };
            match msg {
                Ok(Some((tag, payload))) => {
                    if datachannel::write_msg(&mut consumer_write, tag, &payload)
                        .await
                        .is_err()
                    {
                        return; // consumer gone
                    }
                    // A session-ending ERROR (TX_STALE/PEER_GONE) forwarded from
                    // the source ends the session; the source closes after it, so
                    // stop rather than re-report on the trailing EOF.
                    if tag == datachannel::TAG_ERROR && datachannel::error_ends_session(&payload) {
                        return;
                    }
                }
                // The source's read ended with no terminal frame. If `up` shut
                // the write half because the CONSUMER left, end silently (as a
                // local paste does); otherwise the source genuinely vanished
                // mid-stream → PEER_GONE.
                _ => {
                    if !consumer_gone.load(std::sync::atomic::Ordering::SeqCst) {
                        let _ = datachannel::write_error(&mut consumer_write, "PEER_GONE").await;
                    }
                    return;
                }
            }
        }
    };

    tokio::pin!(up);
    tokio::pin!(down);
    // Race the two, but keep downstream authoritative: if UPSTREAM finishes
    // first, drive downstream to completion so the terminal error is still
    // reported (upstream has shut the write half, so downstream's read unblocks).
    let up_first = tokio::select! {
        _ = down.as_mut() => false,
        _ = up.as_mut() => true,
    };
    if up_first {
        down.await;
    }
}

// ---------------------------------------------------------------------------
// Brick 6: transactions.fill — the Core writes designated targets itself.
// ---------------------------------------------------------------------------

/// Runs a fill: reserves the transaction, writes each target (from the local
/// disk, or relayed from the source), and reports through `transfer.*`.
/// Fire-and-forget like `files.send`; cancelable via `files.cancel`. On failure
/// or cancellation, partial files are left in place — a fill writes the
/// backend's OS-watched paste skeletons directly (no temp+rename possible), and
/// the backend discards whatever `transfer.*` did not confirm.
pub(crate) async fn run_fill(
    state: Arc<AppState>,
    transfer_id: String,
    tx_id: String,
    plan: FillPlan,
    cancel: Arc<Notify>,
) {
    let files_json: Vec<Value> = plan
        .items
        .iter()
        .map(|i| json!({ "name": i.name, "size": i.size }))
        .collect();
    let mut started =
        json!({ "transfer_id": transfer_id, "files": files_json, "total": plan.total });
    if let Some(d) = &plan.device_id {
        started["device_id"] = json!(d);
    }
    dataplane::notify_transfers(&state, "transfer.started", &started);

    // Reserve the transaction against deletion for the whole fill (survives a
    // supersession, like a consumer channel). Gone since the plan was resolved:
    // TX_STALE.
    let mode = state
        .clipboard
        .lock()
        .expect("lock clipboard")
        .begin_session(&tx_id);
    let Some(mode) = mode else {
        finish_fill(&state, &transfer_id, Err("TX_STALE".to_string()));
        return;
    };

    // `biased` + fill FIRST: on a tie, a completed fill is not reported
    // cancelled.
    let outcome = tokio::select! {
        biased;
        r = fill_entries(&state, &tx_id, &mode, &plan, &transfer_id) => r,
        _ = cancel.notified() => Err("cancelled".to_string()),
    };
    state
        .clipboard
        .lock()
        .expect("lock clipboard")
        .end_session(&tx_id);
    finish_fill(&state, &transfer_id, outcome);
}

/// Deregisters the transfer then emits the terminal event ONCE (order matters:
/// a `files.cancel` that saw the outcome and retried finds `TRANSFER_UNKNOWN`).
fn finish_fill(state: &AppState, transfer_id: &str, outcome: Result<Vec<Value>, String>) {
    state
        .transfers
        .lock()
        .expect("lock transfers")
        .entries
        .remove(transfer_id);
    match outcome {
        Ok(paths) => dataplane::notify_transfers(
            state,
            "transfer.finished",
            &json!({ "transfer_id": transfer_id, "paths": paths }),
        ),
        Err(error) => dataplane::notify_transfers(
            state,
            "transfer.failed",
            &json!({ "transfer_id": transfer_id, "error": error }),
        ),
    }
}

/// Writes every target of the fill, returning the written paths or the error
/// string of the first failure (a JSON-RPC-style code, `PEER_GONE`, `TX_STALE`,
/// `FILE_CHANGED`… or a disk error).
async fn fill_entries(
    state: &Arc<AppState>,
    tx_id: &str,
    mode: &ServeMode,
    plan: &FillPlan,
    transfer_id: &str,
) -> Result<Vec<Value>, String> {
    let mut done = 0u64;
    let mut throttle = dataplane::Throttle::new();
    let total = plan.total;
    let mut progress = |delta: u64| {
        done = done.saturating_add(delta);
        throttle.tick(state, transfer_id, done, total);
    };
    progress(0);

    // A remote fill opens one session to the source for all the entries; a local
    // fill reads straight from the disk. (A materialized clip has no files, so a
    // fill never reaches it — it resolves to `Local` and reads nothing.)
    let mut session = match mode {
        ServeMode::Remote { node_id, device_id } => {
            let peer = match dataplane::resolve_peer(state, device_id) {
                Some(p) if p.node_id == *node_id && p.relay_url.is_some() => p,
                _ => return Err("PEER_GONE".to_string()),
            };
            Some(RemoteSession::open(state, &peer, tx_id).await?)
        }
        ServeMode::Local => None,
    };

    let mut written = Vec::with_capacity(plan.items.len());
    for item in &plan.items {
        if let Some(parent) = item.dest_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }
        let mut dest = tokio::fs::File::create(&item.dest_path)
            .await
            .map_err(|e| e.to_string())?;
        match &mut session {
            Some(sess) => {
                sess.read_file(&item.file_id, item.size, &mut dest, &mut progress)
                    .await?
            }
            None => fill_local(state, tx_id, &item.file_id, &mut dest, &mut progress).await?,
        }
        datachannel::bounded(dest.flush())
            .await
            .map_err(|e| e.to_string())?;
        written.push(json!(item.dest_path.to_string_lossy()));
    }
    Ok(written)
}

/// Copies a manifest file from the local disk into `dest` (a local fill: a paste
/// on the very device that copied). Re-verifies the frozen identity first
/// (`FILE_CHANGED`), like a consumer `READ`.
async fn fill_local(
    state: &Arc<AppState>,
    tx_id: &str,
    file_id: &str,
    dest: &mut tokio::fs::File,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), String> {
    let source = {
        let cb = state.clipboard.lock().expect("lock clipboard");
        match cb.lookup_file(tx_id, file_id) {
            crate::clipboard::Lookup::Gone => return Err("TX_STALE".to_string()),
            crate::clipboard::Lookup::NoSuchFile => return Err("FILE_UNKNOWN".to_string()),
            crate::clipboard::Lookup::File(entry) => {
                if !entry.still_matches() {
                    return Err("FILE_CHANGED".to_string());
                }
                match entry.source() {
                    Some(p) => p.to_path_buf(),
                    None => return Err("FILE_CHANGED".to_string()),
                }
            }
        }
    };
    copy_file(&source, dest, progress).await
}

async fn copy_file(
    source: &Path,
    dest: &mut tokio::fs::File,
    progress: &mut (dyn FnMut(u64) + Send),
) -> Result<(), String> {
    let mut file = datachannel::bounded(tokio::fs::File::open(source))
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = datachannel::bounded(file.read(&mut buf))
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        datachannel::bounded(dest.write_all(&buf[..n]))
            .await
            .map_err(|e| e.to_string())?;
        progress(n as u64);
    }
    Ok(())
}

/// A driven `clip_session` to a source: the Core issues `READ`s and consumes the
/// `DATA`/`EOF`/`ERROR` responses itself (as opposed to the transparent pipe).
/// Used by `transactions.fill`.
struct RemoteSession {
    stream: Box<dyn IoStream>,
}

impl RemoteSession {
    async fn open(
        state: &Arc<AppState>,
        peer: &PeerAddr,
        tx_id: &str,
    ) -> Result<RemoteSession, String> {
        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, state.transport.open(peer))
            .await
            .map_err(|_| "PEER_GONE".to_string())?
            .map_err(|_| "PEER_GONE".to_string())?;
        let frame = serde_json::to_vec(&json!({ "type": "clip_session", "tx_id": tx_id }))
            .expect("serialize clip_session");
        dataplane::write_frame(&mut stream, &frame)
            .await
            .map_err(|_| "PEER_GONE".to_string())?;
        Ok(RemoteSession { stream })
    }

    /// Reads the whole file `file_id` (`size` bytes) into `dest`, reporting each
    /// chunk. An `ERROR` frame surfaces its code; a stream that ends without an
    /// `EOF` is `PEER_GONE`.
    async fn read_file(
        &mut self,
        file_id: &str,
        size: u64,
        dest: &mut tokio::fs::File,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<(), String> {
        let req = json!({ "file_id": file_id, "offset": 0, "len": size });
        let req = serde_json::to_vec(&req).expect("serialize READ");
        datachannel::write_msg(&mut self.stream, datachannel::TAG_READ, &req)
            .await
            .map_err(|_| "PEER_GONE".to_string())?;
        loop {
            match datachannel::bounded(datachannel::read_msg(&mut self.stream)).await {
                Ok(Some((datachannel::TAG_DATA, payload))) if payload.len() >= 8 => {
                    let bytes = &payload[8..];
                    datachannel::bounded(dest.write_all(bytes))
                        .await
                        .map_err(|e| e.to_string())?;
                    progress(bytes.len() as u64);
                }
                Ok(Some((datachannel::TAG_EOF, _))) => return Ok(()),
                Ok(Some((datachannel::TAG_ERROR, payload))) => {
                    let code = serde_json::from_slice::<Value>(&payload)
                        .ok()
                        .and_then(|v| v["code"].as_str().map(str::to_string))
                        .unwrap_or_else(|| "PEER_GONE".to_string());
                    return Err(code);
                }
                _ => return Err("PEER_GONE".to_string()),
            }
        }
    }
}

fn timed_out(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, format!("timed out: {what}"))
}
