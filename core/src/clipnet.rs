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

use crate::clipboard::{FillPlan, Origin, Producer, ServeMode, Transaction};
use crate::connector::IoStream;
use crate::datachannel;
use crate::dataplane::{self, PeerAddr};
use crate::rpc::RpcErr;
use crate::state::{AppState, ConnId};

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
        producer: crate::clipboard::Producer::Clipboard,
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
/// carrying the announce metadata then the inline blobs. `blobs` is the
/// per-format bytes; sharing them across the per-peer tasks is a cheap `Arc`
/// clone.
///
/// Returns how many devices the push was launched to — the announce's
/// `pushed_to`. Unlike `propagate`, this is not fire-and-forget from the
/// caller's point of view: when the count is non-zero, the fan-out is awaited
/// and settles into exactly one `clipboard.pushed` notification on the
/// ANNOUNCING connection (`{tx_id, delivered, failed}`). A materialized copy
/// comes from a source that is only briefly alive — a phone whose user shared a
/// snippet and walked away — so "did my devices actually get it?" is the one
/// question it cannot answer by staying around, and the one an Android share
/// sheet must answer before it stops holding the process up.
pub(crate) fn propagate_materialized(
    state: &Arc<AppState>,
    announce: Value,
    blobs: crate::clipboard::MaterializedBlobs,
    announcer: ConnId,
    tx_id: String,
) -> usize {
    let peers = dataplane::account_peers(state);
    if peers.is_empty() {
        return 0;
    }
    // Only the METADATA frame is bounded by `MAX_FRAME`; the blobs stream
    // separately (capped by `MATERIALIZE_MAX`). An inline announce is tiny, so
    // this never fires in practice — kept for parity with `propagate`. Reported
    // as "launched to nobody": nothing left the device, so no report follows.
    let serialized = serde_json::to_vec(&announce).map_or(usize::MAX, |b| b.len());
    if serialized + 64 > dataplane::MAX_FRAME as usize {
        tracing::warn!("clipboard materialized metadata too large to propagate; it stays local");
        return 0;
    }
    let launched = peers.len();
    let blobs = Arc::new(blobs);
    let state = state.clone();
    tokio::spawn(async move {
        /// How one peer's push ended. Ordered by what the report says about
        /// it: a policy refusal is a failure WITH its own remedy, so it is
        /// counted apart (`no_direct_path` in the report) instead of blending
        /// into "could not reach", whose remedy points the wrong way.
        enum Push {
            Delivered,
            Failed,
            NoDirectPath,
        }
        let mut pushes = tokio::task::JoinSet::new();
        for peer in peers {
            let state = state.clone();
            let announce = announce.clone();
            let blobs = blobs.clone();
            pushes.spawn(async move {
                match send_push(&state, &peer, &announce, &blobs).await {
                    Ok(()) => Push::Delivered,
                    Err(e) if dataplane::failure_code(&e) == crate::dataplane::NO_DIRECT_PATH => {
                        // The relays may not carry the bytes, but introducing
                        // is exactly what they are for: fall back to a
                        // metadata-only announce, so the destination learns
                        // the clip exists and its paste speaks the policy's
                        // own code (or rides a direct path if one forms)
                        // instead of silently serving the previous clip.
                        if let Err(e) = send_announce(&state, &peer, &announce).await {
                            tracing::debug!(peer = %peer.node_id, error = %e,
                                "fallback announce after a rendezvous-only refusal not delivered");
                        }
                        Push::NoDirectPath
                    }
                    Err(e) => {
                        tracing::debug!(peer = %peer.node_id, error = %e, "materialized clip not pushed");
                        Push::Failed
                    }
                }
            });
        }
        let mut delivered = 0usize;
        let mut failed = 0usize;
        let mut no_direct_path = 0usize;
        while let Some(outcome) = pushes.join_next().await {
            // A push task that panicked counts as a failure, never a delivery:
            // the report must never over-promise.
            match outcome {
                Ok(Push::Delivered) => delivered += 1,
                Ok(Push::NoDirectPath) => {
                    failed += 1;
                    no_direct_path += 1;
                }
                Ok(Push::Failed) | Err(_) => failed += 1,
            }
        }
        // The announcer may already be gone (that is the whole point of
        // push-at-copy), `notify_conn` is then a no-op. `no_direct_path`
        // counts the subset of `failed` the announced relay role refused
        // (#88): those devices are online and introduced, only the bytes
        // need a direct path, and the share sheet words that remedy.
        state.registry.lock().expect("lock registry").notify_conn(
            announcer,
            "clipboard.pushed",
            &json!({
                "tx_id": tx_id,
                "delivered": delivered,
                "failed": failed,
                "no_direct_path": no_direct_path,
            }),
        );
    });
    launched
}

/// Source half of a `clip_push`: opens a stream, writes the announce frame
/// (`type: clip_push`), streams each inline format's bytes as `DATA*`+`EOF` in
/// `formats` order, then waits for the receiver's ack and closes. The receiver
/// knows each blob's length from `formats[].size` (made exact at the announce),
/// so no per-blob header is needed.
///
/// The ack is REQUIRED here (unlike `send_announce`, which discards it): it is
/// what makes `delivered` in the push report mean "a device has the bytes"
/// rather than "we wrote into a void". A peer too old to know `clip_push`
/// abandons the stream without acking, and is reported as a failure instead of
/// silently passing for a delivery.
async fn send_push(
    state: &Arc<AppState>,
    peer: &PeerAddr,
    announce: &Value,
    blobs: &[(String, Arc<Vec<u8>>)],
) -> std::io::Result<()> {
    // Sized open (#88): the blobs about to be streamed are the payload, and
    // under a rendezvous-only announcement an over-cap push needs a direct
    // path (a failure is one counter in the push report, like any other).
    let payload = blobs
        .iter()
        .fold(0u64, |a, (_, bytes)| a.saturating_add(bytes.len() as u64));
    let mut stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        state.transport.open_for_payload(peer, payload),
    )
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
    let ack = tokio::time::timeout(ANNOUNCE_ACK_TIMEOUT, dataplane::read_frame(&mut stream))
        .await
        .map_err(|_| timed_out("clip_push ack"))??;
    let acked = serde_json::from_slice::<Value>(&ack)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(Value::as_str)
                .map(|t| t == "clip_ack")
        })
        .unwrap_or(false);
    if !acked {
        return Err(datachannel::unexpected("materialize: no clip_ack"));
    }
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
        // A re-enrolled source (new node_id) is no longer the device that made
        // this offer; one with no route to it (no published relay, not seen on
        // the LAN) is unreachable.
        Some(p) if p.node_id == node_id && dataplane::peer_reachable(state, &p) => p,
        _ => {
            let _ = datachannel::write_error(&mut consumer_write, "PEER_GONE").await;
            return;
        }
    };
    // Sized open (#88): the bound of what this pipe can relay. A refusal by
    // the announced relay role is its own code - "no route" would send the
    // user chasing the wrong remedy.
    let payload = state
        .clipboard
        .lock()
        .expect("lock clipboard")
        .payload_bound(tx_id);
    let net = tokio::time::timeout(
        CONNECT_TIMEOUT,
        state.transport.open_for_payload(&peer, payload),
    )
    .await;
    let mut net = match net {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let code = dataplane::failure_code(&e);
            let code = if code == crate::dataplane::NO_DIRECT_PATH {
                code
            } else {
                "PEER_GONE".to_string()
            };
            let _ = datachannel::write_error(&mut consumer_write, &code).await;
            return;
        }
        Err(_) => {
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
        // The reset `Notify` wakes EVERY session (Core stop, logout, or a
        // targeted `transactions.revoke`); only the sessions whose entry is
        // gone are cut. `notify_waiters` stores no permit, so the waiter must
        // be LATCHED across every await in this loop (the relay write to the
        // consumer included): pinned, enabled before the first liveness
        // check, and re-armed BEFORE each re-check - a wake landing in any
        // gap is then seen either by the check or by the fresh waiter.
        let mut reset = std::pin::pin!(state.clipboard_reset.notified());
        reset.as_mut().enable();
        if !state
            .clipboard
            .lock()
            .expect("lock clipboard")
            .is_live(tx_id)
        {
            let _ = datachannel::write_error(&mut consumer_write, "TX_STALE").await;
            return;
        }
        loop {
            // One frame from the source, KEPT IN FLIGHT across wakes for
            // someone else's revocation: `read_msg` is not cancel-safe, and
            // dropping it mid-frame would desynchronize the relay.
            let mut read =
                std::pin::pin!(datachannel::bounded(datachannel::read_msg(&mut net_read)));
            let msg = loop {
                tokio::select! {
                    biased;
                    _ = reset.as_mut() => {
                        reset.set(state.clipboard_reset.notified());
                        reset.as_mut().enable();
                        if state.clipboard.lock().expect("lock clipboard").is_live(tx_id) {
                            continue;
                        }
                        let _ = datachannel::write_error(&mut consumer_write, "TX_STALE").await;
                        return;
                    }
                    m = read.as_mut() => break m,
                }
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
                    // stop rather than re-report on the trailing EOF. A relayed
                    // TX_STALE also evicts an ADOPTED entry: the source no
                    // longer backs the promise the local record makes.
                    if tag == datachannel::TAG_ERROR && datachannel::error_ends_session(&payload) {
                        if error_code(&payload).as_deref() == Some("TX_STALE") {
                            evict_stale_published(state, tx_id);
                        }
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

    // A fill is a session: a revocation must cut it too, not only the consumer
    // channels. The watcher latches the reset wake (pinned + enabled, liveness
    // checked before each park so a wake in the re-arm gap is never lost) and
    // resolves only when OUR transaction is gone - a wake for someone else's
    // revocation parks again.
    let revoked = async {
        let mut reset = std::pin::pin!(state.clipboard_reset.notified());
        reset.as_mut().enable();
        loop {
            if !state
                .clipboard
                .lock()
                .expect("lock clipboard")
                .is_live(&tx_id)
            {
                return;
            }
            reset.as_mut().await;
            reset.set(state.clipboard_reset.notified());
            reset.as_mut().enable();
        }
    };
    // `biased` + fill FIRST: on a tie, a completed fill is not reported
    // cancelled (nor cut by a revocation that lost the race).
    let outcome = tokio::select! {
        biased;
        r = fill_entries(&state, &tx_id, &mode, &plan, &transfer_id) => r,
        _ = revoked => Err("TX_STALE".to_string()),
        _ = cancel.notified() => Err("cancelled".to_string()),
    };
    state
        .clipboard
        .lock()
        .expect("lock clipboard")
        .end_session(&tx_id);
    // A fill that died on TX_STALE evicts an ADOPTED entry, like the consumer
    // pipe: the source declared the id stale, the local record is a promise
    // nobody backs. (A local TX_STALE means the entry is already gone: no-op.)
    if matches!(&outcome, Err(e) if e == "TX_STALE") {
        evict_stale_published(&state, &tx_id);
    }
    finish_fill(&state, &transfer_id, outcome);
}

/// Removes a published entry whose source declared it stale, and cuts any
/// sibling session still relaying it (the reset `Notify`, which every survivor
/// re-checks). No-op on a clipboard transaction or an already-gone id.
fn evict_stale_published(state: &AppState, tx_id: &str) {
    let removed = state
        .clipboard
        .lock()
        .expect("lock clipboard")
        .evict_published(tx_id);
    if removed {
        state.clipboard_reset.notify_waiters();
    }
}

/// The `code` of a data-channel `ERROR` payload, if it parses as one.
fn error_code(payload: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|v| v["code"].as_str().map(str::to_string))
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
                Some(p) if p.node_id == *node_id && dataplane::peer_reachable(state, &p) => p,
                _ => return Err("PEER_GONE".to_string()),
            };
            Some(RemoteSession::open(state, &peer, tx_id, plan.total).await?)
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
        payload: u64,
    ) -> Result<RemoteSession, String> {
        // Sized open (#88): the fill's total is the payload. A refusal by the
        // announced relay role keeps its own code through the fill's string
        // errors - it has a remedy of its own, unlike "the peer is gone".
        let mut stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            state.transport.open_for_payload(peer, payload),
        )
        .await
        .map_err(|_| "PEER_GONE".to_string())?
        .map_err(|e| {
            let code = dataplane::failure_code(&e);
            if code == crate::dataplane::NO_DIRECT_PATH {
                code
            } else {
                "PEER_GONE".to_string()
            }
        })?;
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

// ---------------------------------------------------------------------------
// Published transactions across devices: `tx_fetch` / `transactions.adopt`
// (doc/core-api.md, "Transactions" - the long-lived producer, #83).
// ---------------------------------------------------------------------------

/// Whole-exchange budget for an adopt (connect + one frame each way): the
/// house exchange norm, well under the caller's patience and the callers'
/// serialization of their own connection.
const ADOPT_TIMEOUT: Duration = Duration::from_secs(10);

/// Source side of a `tx_fetch` (a peer's `transactions.adopt`): answers the
/// frozen record of a PUBLISHED transaction, or `TX_STALE` - a clipboard
/// transaction is not adoptable (its metadata travels by `clip_announce`), and
/// an unknown id answers the same, disclosing nothing. The peer is already
/// attested (C7 ran in `serve`); possession of the unguessable `tx_id` is the
/// authorization, as everywhere on this plane.
pub(crate) async fn serve_tx_fetch(
    state: Arc<AppState>,
    first: Value,
    mut stream: Box<dyn IoStream>,
) {
    let stale = json!({ "type": "tx_error", "code": "TX_STALE" });
    let reply = match first.get("tx_id").and_then(Value::as_str) {
        None => stale.clone(),
        Some(tx_id) => {
            let record = state
                .clipboard
                .lock()
                .expect("lock clipboard")
                .published_record(tx_id);
            match record {
                Some(mut record) => {
                    record["type"] = json!("tx_manifest");
                    record
                }
                None => stale.clone(),
            }
        }
    };
    let Ok(mut bytes) = serde_json::to_vec(&reply) else {
        return;
    };
    // The reply must fit one control frame: a manifest too large to travel is
    // its own honest refusal - the v1 bound lazy enumeration will lift, same
    // as a clip too large to propagate. Headroom as in `propagate`.
    if bytes.len() + 64 > dataplane::MAX_FRAME as usize {
        bytes = serde_json::to_vec(&json!({ "type": "tx_error", "code": "MANIFEST_TOO_LARGE" }))
            .expect("serialize tx_error");
    }
    // The reply can be a whole manifest: a peer that never grants stream
    // credit must not pin this handler's slot forever (the accept loop's
    // budget) - the house no-progress bound, as every responder applies.
    if datachannel::bounded(dataplane::write_frame(&mut stream, &bytes))
        .await
        .is_err()
    {
        return;
    }
    // QUIC lifecycle: hold the reply until the initiator closes.
    let _ = stream.shutdown().await;
    let _ = tokio::time::timeout(LINGER, dataplane::drain(&mut stream)).await;
}

/// Destination side of `transactions.adopt`: fetches the frozen manifest of a
/// transaction PUBLISHED on `device_id` and installs it locally - outside the
/// election, owned by the adopting connection - so the untouched consumer
/// machinery (`transactions.open`/`transactions.fill`, the data channel) then
/// serves it exactly like a remote clip. Errors follow `files.send`'s
/// targeting doctrine: `DEVICE_UNKNOWN` (absent, or attested under a foreign
/// key - indistinguishable, fail-closed), `DEVICE_OFFLINE` (no route known, or
/// nothing usable answered in time), `TX_STALE` (the source does not back that
/// id - or answered something that is not a valid manifest: nothing installs),
/// and `MANIFEST_TOO_LARGE` relayed as itself.
pub(crate) async fn adopt(
    state: &Arc<AppState>,
    owner: ConnId,
    device_id: &str,
    tx_id: &str,
) -> Result<Value, RpcErr> {
    let peer =
        dataplane::resolve_peer(state, device_id).ok_or_else(|| RpcErr::app("DEVICE_UNKNOWN"))?;
    if !dataplane::peer_reachable(state, &peer) {
        return Err(RpcErr::app("DEVICE_OFFLINE"));
    }
    let reply = tokio::time::timeout(ADOPT_TIMEOUT, fetch_manifest(state, &peer, tx_id))
        .await
        .map_err(|_| RpcErr::app("DEVICE_OFFLINE"))?
        .map_err(|_| RpcErr::app("DEVICE_OFFLINE"))?;
    match reply.get("type").and_then(Value::as_str) {
        Some("tx_manifest") => {}
        Some("tx_error") => {
            // Only the codes this exchange defines travel through; anything
            // else collapses into the fail-closed refusal.
            let code = match reply.get("code").and_then(Value::as_str) {
                Some("MANIFEST_TOO_LARGE") => "MANIFEST_TOO_LARGE",
                _ => "TX_STALE",
            };
            // The source's own word that it no longer backs the id: a record
            // this Core already installed (a re-adopt probing after a
            // reconnect) is a promise nobody backs - evict it, exactly as a
            // relayed TX_STALE would.
            if code == "TX_STALE" {
                evict_stale_published(state, tx_id);
            }
            return Err(RpcErr::app(code));
        }
        _ => return Err(RpcErr::app("TX_STALE")),
    }
    // Fail-closed manifest re-validation, exactly like a remote clip announce
    // (`validate_remote_manifest`): a hostile record installs nothing. The
    // local entry is rebuilt from what WE asked and verified - the requested
    // `tx_id`, the resolved peer - never from the reply's own claims.
    let files = reply
        .get("files")
        .and_then(Value::as_array)
        .and_then(|a| crate::clipboard::validate_remote_manifest(a))
        .filter(|f| !f.is_empty())
        .ok_or_else(|| RpcErr::app("TX_STALE"))?;
    let tx = Transaction {
        tx_id: tx_id.to_string(),
        device_id: Some(device_id.to_string()),
        seq: 0, // outside the election: never compared
        formats: Vec::new(),
        files,
        sensitive: false,
        producer: Producer::Published {
            owners: vec![owner],
        },
        origin: Origin::Remote {
            node_id: peer.node_id.clone(),
            device_id: device_id.to_string(),
        },
        superseded: false,
        sessions: 0,
        materialized: HashMap::new(),
    };
    state
        .clipboard
        .lock()
        .expect("lock clipboard")
        .install_adopted(tx)
}

/// One `tx_fetch` round-trip: open, ask, read the one reply, close.
async fn fetch_manifest(
    state: &Arc<AppState>,
    peer: &PeerAddr,
    tx_id: &str,
) -> std::io::Result<Value> {
    let mut stream = state.transport.open(peer).await?;
    let frame = serde_json::to_vec(&json!({ "type": "tx_fetch", "tx_id": tx_id }))?;
    dataplane::write_frame(&mut stream, &frame).await?;
    let reply = dataplane::read_frame(&mut stream).await?;
    // Initiator closes after reading the reply (QUIC lifecycle).
    let _ = stream.shutdown().await;
    serde_json::from_slice(&reply).map_err(|_| datachannel::unexpected("tx_fetch reply"))
}

fn timed_out(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, format!("timed out: {what}"))
}
