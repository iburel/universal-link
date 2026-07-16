// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The data channel: payloads never ride the control plane (doc/core-api.md,
//! "The data channel"). A component opens a SECOND connection to the same
//! socket, presents a `channel_token`, and the connection becomes a binary
//! protocol carrying file ranges and inline blobs.
//!
//! Routing: `conn::run` reads the first frame; an attach frame (an LSP frame
//! `{ "channel_token": "…" }`, no `method`) hands the connection here. The
//! token is single-use, unguessable, and bound to the component it was minted
//! for (peer credentials) — the data connection carries no `hello`, so the
//! token IS its credential.
//!
//! # Wire format (frozen here)
//!
//! After the attach frame, each message is `u32` big-endian length `L`, then
//! `L` bytes: one tag byte, then the payload.
//! - Consumer → Core: `READ` (`{ file_id, offset, len }`), `FETCH`
//!   (`{ format }`), `ABORT`.
//! - Core → consumer (and provider → Core): `DATA` (8-byte big-endian offset,
//!   then raw bytes), `EOF`, `ERROR` (`{ code }`).
//!
//! Every request is answered by `DATA*` then `EOF`; `EOF` terminates the
//! RESPONSE, not the file (a `READ` past the end returns the intersection then
//! `EOF`). `ERROR` ends only the request — the channel stays usable — except
//! `TX_STALE` and `PEER_GONE`, which end the session.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::clipboard::{InlineSource, Lookup};
use crate::rpc::RpcErr;
use crate::state::{AppState, ChannelGrant, ChannelKind};
use crate::transport::PeerInfo;

/// What a provider channel forwards from the source backend to the waiting
/// consumer `FETCH`: the inline blob in chunks, then its end (or a failure).
pub(crate) enum ProviderMsg {
    Data(Vec<u8>),
    Eof,
    Error(String),
}

/// Total time budget for an inline `FETCH` round-trip (issue `clipboard.get_data`,
/// the backend opens a provider channel, streams the blob, replies). Bounds a
/// backend that acknowledges but never delivers.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

// Consumer → Core. (`pub(crate)`: the network relay in `clipnet` speaks the
// very same binary protocol over a `clip_session` stream.)
pub(crate) const TAG_READ: u8 = 0x01;
pub(crate) const TAG_FETCH: u8 = 0x02;
const TAG_ABORT: u8 = 0x03;
// Core → consumer, and provider → Core.
pub(crate) const TAG_DATA: u8 = 0x10;
pub(crate) const TAG_EOF: u8 = 0x11;
pub(crate) const TAG_ERROR: u8 = 0x12;

/// Bounds a single message: control messages are tiny, and a provider's `DATA`
/// chunk is bounded by the sender — a peer never chooses our allocation.
const MAX_MSG: usize = 1024 * 1024 + 16;
/// The file-reading buffer; a large range streams through it without ever being
/// held whole in memory.
const CHUNK: usize = 64 * 1024;
/// No-progress budget on a read or write. Not a cap on total duration: a large
/// range takes as long as the bytes keep flowing, but a peer that stops driving
/// is swept rather than pinning the connection forever.
pub(crate) const STALL: Duration = Duration::from_secs(30);

/// Serves a data-channel connection: validates the token, binds it to the peer,
/// then serves according to the channel kind. Anything unexpected closes the
/// connection (fail-closed — the data channel owes no interpretable reply).
pub(crate) async fn run<R, W>(state: Arc<AppState>, reader: R, write: W, peer: PeerInfo, token: String)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let grant = state
        .registry
        .lock()
        .expect("lock registry")
        .take_channel_token(&token);
    let Some(grant) = grant else {
        // Unknown, already-used, or expired token: no reply owed.
        return;
    };
    // Bound to the minting component: when both platforms expose a pid, they
    // must match — a different same-user process that observed the token is
    // turned away. When a pid is unavailable (macOS), the unguessable token is
    // the sole credential.
    if let (Some(minted), Some(seen)) = (grant.pid, peer.pid)
        && minted != seen
    {
        return;
    }
    match grant.kind {
        ChannelKind::Consumer => serve_consumer(&state, reader, write, grant.tx_id).await,
        // Provider: the backend pushes the inline blob it was asked for; we
        // forward it to the consumer that is waiting on the sink.
        ChannelKind::Provider => {
            if let Some(sink) = grant.sink {
                serve_provider(reader, sink).await;
            }
        }
    }
}

/// Reads the backend's blob off a provider channel and forwards it to the
/// waiting `FETCH`. Provider → Core only (the backend never reads here). When
/// the stream ends — `EOF`, `ERROR`, or a dropped connection — the sink closes.
async fn serve_provider<R>(mut reader: R, sink: tokio::sync::mpsc::Sender<ProviderMsg>)
where
    R: AsyncRead + Unpin,
{
    loop {
        match bounded(read_msg(&mut reader)).await {
            Ok(Some((TAG_DATA, payload))) if payload.len() >= 8 => {
                if sink.send(ProviderMsg::Data(payload[8..].to_vec())).await.is_err() {
                    break; // the consumer gave up
                }
            }
            Ok(Some((TAG_EOF, _))) => {
                let _ = sink.send(ProviderMsg::Eof).await;
                break;
            }
            Ok(Some((TAG_ERROR, payload))) => {
                let code = serde_json::from_slice::<Value>(&payload)
                    .ok()
                    .and_then(|v| v["code"].as_str().map(str::to_string))
                    .unwrap_or_else(|| "CLIP_STALE".to_string());
                let _ = sink.send(ProviderMsg::Error(code)).await;
                break;
            }
            // EOF with no trailing `EOF` frame, stall, or a bad frame: the sink
            // closes and the consumer settles on the `get_data` reply.
            _ => break,
        }
    }
}

/// A paste session: reserves the transaction (it cannot be deleted while read),
/// serves it according to its origin, and releases it at the end whatever the
/// reason. A LOCAL clip is served here (disk ranges + inline pulls from the
/// announcer); a REMOTE clip is relayed to its source device (`clipnet`) — the
/// same entry point on both the destination Core (a local component's channel)
/// and the source Core (an incoming `clip_session` stream, always local there).
pub(crate) async fn serve_consumer<R, W>(
    state: &Arc<AppState>,
    reader: R,
    mut write: W,
    tx_id: String,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // The transaction may have been deleted between `transactions.open` and
    // this attach: no session to serve.
    let origin = state
        .clipboard
        .lock()
        .expect("lock clipboard")
        .begin_session(&tx_id);
    let Some(origin) = origin else {
        let _ = write_error(&mut write, "TX_STALE").await;
        return;
    };
    match origin {
        crate::clipboard::Origin::Local { .. } => serve_local(state, reader, write, &tx_id).await,
        crate::clipboard::Origin::Remote { node_id, device_id } => {
            crate::clipnet::pipe_consumer(state, reader, write, &tx_id, &node_id, &device_id).await;
        }
    }
    state
        .clipboard
        .lock()
        .expect("lock clipboard")
        .end_session(&tx_id);
}

/// Serves a LOCAL clip: the consumer drives (`READ`/`FETCH`), one request at a
/// time, reading file ranges from this disk and pulling inline blobs from the
/// announcing backend.
async fn serve_local<R, W>(state: &Arc<AppState>, mut reader: R, mut write: W, tx_id: &str)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let (tag, payload) = tokio::select! {
            // A clipboard-wide reset (Core stop, logout): the transaction is
            // gone — cut the session with `TX_STALE`.
            _ = state.clipboard_reset.notified() => {
                let _ = write_error(&mut write, "TX_STALE").await;
                break;
            }
            msg = bounded(read_msg(&mut reader)) => match msg {
                Ok(Some(msg)) => msg,
                // EOF (channel closed = paste abandoned), stall, or framing
                // violation: end the session.
                _ => break,
            },
        };
        let ended = match tag {
            TAG_READ => handle_read(state, tx_id, &payload, &mut write).await,
            TAG_FETCH => handle_fetch(state, tx_id, &payload, &mut write).await,
            // Nothing is ever in flight between two requests (the consumer waits
            // for our EOF): an ABORT here has nothing to cancel.
            TAG_ABORT => Ok(false),
            _ => Err(unexpected("unknown data-channel tag")),
        };
        match ended {
            Ok(false) => {}
            // Session ended cleanly (TX_STALE), or a write failed (peer gone):
            // either way we stop.
            Ok(true) | Err(_) => break,
        }
    }
}

/// Serves a `READ`: a byte range of a manifest file, from the disk. Returns
/// whether the SESSION must end (only `TX_STALE` does). A malformed request, or
/// a write failure, is an `Err` that closes the channel.
async fn handle_read<W>(
    state: &AppState,
    tx_id: &str,
    payload: &[u8],
    write: &mut W,
) -> std::io::Result<bool>
where
    W: AsyncWrite + Unpin,
{
    let req: Value = serde_json::from_slice(payload).map_err(|_| unexpected("bad READ"))?;
    let file_id = req["file_id"].as_str().ok_or_else(|| unexpected("READ file_id"))?;
    let offset = req["offset"].as_u64().ok_or_else(|| unexpected("READ offset"))?;
    let len = req["len"].as_u64().ok_or_else(|| unexpected("READ len"))?;

    let lookup = state
        .clipboard
        .lock()
        .expect("lock clipboard")
        .lookup_file(tx_id, file_id);
    match lookup {
        // The transaction is gone (deleted after supersession, or dropped at
        // logout): the session cannot continue.
        Lookup::Gone => {
            write_error(write, "TX_STALE").await?;
            Ok(true)
        }
        // A directory carries the tree, not bytes; an unknown id: request-scoped
        // refusal, the channel survives for other reads.
        Lookup::NoSuchFile => {
            write_error(write, "FILE_UNKNOWN").await?;
            Ok(false)
        }
        Lookup::File(entry) if entry.is_dir => {
            write_error(write, "FILE_UNKNOWN").await?;
            Ok(false)
        }
        Lookup::File(entry) => {
            // The frozen file must still be itself: a swap, a same-size rewrite,
            // or a vanished file fails the read rather than serving other bytes.
            // `still_matches` also fails a remote entry (no local backing) — this
            // path only ever runs for a local clip, so `source()` is then `Some`.
            if !entry.still_matches() {
                write_error(write, "FILE_CHANGED").await?;
                return Ok(false);
            }
            match entry.source() {
                Some(source) => stream_range(write, source, offset, len).await?,
                None => write_error(write, "FILE_CHANGED").await?,
            }
            Ok(false)
        }
    }
}

/// Serves a `FETCH` (a whole inline blob). Pull-at-paste: the Core does not hold
/// the bytes — it asks the announcing backend (`clipboard.get_data`), which
/// streams them over a provider channel that we relay to this consumer. Returns
/// whether the SESSION ends (only `TX_STALE` does); other failures
/// (`FORMAT_UNKNOWN`, `CLIP_STALE`) are request-scoped.
async fn handle_fetch<W>(
    state: &AppState,
    tx_id: &str,
    payload: &[u8],
    write: &mut W,
) -> std::io::Result<bool>
where
    W: AsyncWrite + Unpin,
{
    let req: Value = serde_json::from_slice(payload).map_err(|_| unexpected("bad FETCH"))?;
    let format = req["format"].as_str().ok_or_else(|| unexpected("FETCH format"))?;

    // Resolve then release the lock — the branches below await.
    let source = state
        .clipboard
        .lock()
        .expect("lock clipboard")
        .inline_source(tx_id, format);
    let announcer = match source {
        InlineSource::Gone => {
            write_error(write, "TX_STALE").await?;
            return Ok(true);
        }
        InlineSource::NoFormat => {
            write_error(write, "FORMAT_UNKNOWN").await?;
            return Ok(false);
        }
        InlineSource::Announcer(conn_id) => conn_id,
    };

    // The backend will push the blob over a provider channel identified by this
    // token; we hold the receiving end.
    let (sink, mut rx) = tokio::sync::mpsc::channel(8);
    let (token, issued) = {
        let mut reg = state.registry.lock().expect("lock registry");
        // Bind the provider token to the announcer's process, like the consumer
        // token is bound to its opener.
        let announcer_pid = reg.conns.get(&announcer).and_then(|e| e.pid);
        let token = reg.mint_channel_token(ChannelGrant {
            tx_id: tx_id.to_string(),
            kind: ChannelKind::Provider,
            pid: announcer_pid,
            conn_id: announcer,
            sink: Some(sink),
        });
        let issued = reg.issue_request(
            announcer,
            "clipboard.get_data",
            json!({ "tx_id": tx_id, "format": format, "channel_token": token.clone() }),
        );
        (token, issued)
    };
    let Some((req_id, mut reply_rx)) = issued else {
        // The announcer is gone: no one can vouch for this generation.
        state.registry.lock().expect("lock registry").take_channel_token(&token);
        write_error(write, "CLIP_STALE").await?;
        return Ok(false);
    };

    let relayed = tokio::time::timeout(FETCH_TIMEOUT, relay_inline(write, &mut rx, &mut reply_rx))
        .await
        .unwrap_or_else(|_| write_error_sync("CLIP_STALE"));
    // Reclaim the single-use token (if the provider never attached) and the
    // request waiter (we settled on the channel outcome, not the reply).
    {
        let mut reg = state.registry.lock().expect("lock registry");
        reg.take_channel_token(&token);
        reg.cancel_request(announcer, req_id);
    }
    match relayed {
        Relay::Continue => Ok(false),
        Relay::Broken(e) => Err(e),
        // The relay wants to emit an error but timed out doing so; surface it.
        Relay::LateError(code) => {
            write_error(write, &code).await?;
            Ok(false)
        }
    }
}

/// Outcome of relaying an inline blob to the consumer.
enum Relay {
    /// A full response (`DATA*`+`EOF`) or a request-scoped `ERROR` was written;
    /// the channel stays usable.
    Continue,
    /// A write to the consumer failed (peer gone): the channel is dead.
    Broken(std::io::Error),
    /// The round-trip timed out before anything was written; emit this code.
    LateError(String),
}

fn write_error_sync(code: &str) -> Relay {
    Relay::LateError(code.to_string())
}

/// Relays the backend's blob to the consumer: `DATA*` then `EOF`, or a single
/// `ERROR`. The bytes arrive over `rx` (the provider channel); the `get_data`
/// reply is the completion/verdict signal — an error there (or a lost announcer)
/// is a `CLIP_STALE`.
async fn relay_inline<W>(
    write: &mut W,
    rx: &mut tokio::sync::mpsc::Receiver<ProviderMsg>,
    reply_rx: &mut tokio::sync::oneshot::Receiver<Result<Value, RpcErr>>,
) -> Relay
where
    W: AsyncWrite + Unpin,
{
    let mut offset = 0u64;
    let mut reply_seen = false;
    // Once the provider channel has closed we must STOP polling `rx` (a closed
    // mpsc is perpetually `Ready(None)`): the `biased` select would otherwise
    // spin instead of parking on the reply.
    let mut rx_open = true;
    loop {
        tokio::select! {
            biased;
            msg = rx.recv(), if rx_open => match msg {
                Some(ProviderMsg::Data(bytes)) => {
                    if let Err(e) = write_data(write, offset, &bytes).await {
                        return Relay::Broken(e);
                    }
                    offset += bytes.len() as u64;
                }
                Some(ProviderMsg::Eof) => {
                    return finish(write_msg(write, TAG_EOF, &[]).await);
                }
                Some(ProviderMsg::Error(code)) => {
                    return finish(write_error(write, &code).await);
                }
                // Provider closed without an `EOF`: if the backend already
                // answered, settle on `CLIP_STALE`; otherwise stop polling `rx`
                // and wait for the reply (the verdict).
                None if reply_seen => return finish(write_error(write, "CLIP_STALE").await),
                None => rx_open = false,
            },
            reply = &mut *reply_rx, if !reply_seen => {
                reply_seen = true;
                match reply {
                    // The backend acknowledged AND the blob is still streaming:
                    // keep draining `rx` for the trailing `EOF`.
                    Ok(Ok(_)) if rx_open => {}
                    // Refused (`CLIP_STALE`, usually without opening the
                    // channel), disconnected, or acknowledged after the channel
                    // already closed with no `EOF` (incomplete): all `CLIP_STALE`.
                    _ => return finish(write_error(write, "CLIP_STALE").await),
                }
            }
        }
    }
}

fn finish(result: std::io::Result<()>) -> Relay {
    match result {
        Ok(()) => Relay::Continue,
        Err(e) => Relay::Broken(e),
    }
}

/// Streams `[offset, offset+len)` of `source`, clamped to the file's end, as
/// `DATA` chunks followed by `EOF`. Reading past the end yields the
/// intersection (possibly nothing) then `EOF` — never an error.
async fn stream_range<W>(
    write: &mut W,
    source: &std::path::Path,
    offset: u64,
    len: u64,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut file = bounded(tokio::fs::File::open(source)).await?;
    // Seeking beyond the end is legal; the subsequent read then returns 0.
    bounded(file.seek(std::io::SeekFrom::Start(offset))).await?;
    let mut remaining = len;
    let mut pos = offset;
    let mut buf = vec![0u8; CHUNK];
    while remaining > 0 {
        let want = remaining.min(CHUNK as u64) as usize;
        let n = bounded(file.read(&mut buf[..want])).await?;
        if n == 0 {
            break; // reached the end of the file
        }
        write_data(write, pos, &buf[..n]).await?;
        pos += n as u64;
        remaining -= n as u64;
    }
    write_msg(write, TAG_EOF, &[]).await
}

// ---------------------------------------------------------------------------
// Wire helpers.
// ---------------------------------------------------------------------------

pub(crate) async fn write_data<W: AsyncWrite + Unpin>(
    write: &mut W,
    offset: u64,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut payload = Vec::with_capacity(8 + bytes.len());
    payload.extend_from_slice(&offset.to_be_bytes());
    payload.extend_from_slice(bytes);
    write_msg(write, TAG_DATA, &payload).await
}

pub(crate) async fn write_error<W: AsyncWrite + Unpin>(
    write: &mut W,
    code: &str,
) -> std::io::Result<()> {
    let payload = serde_json::to_vec(&json!({ "code": code })).expect("serialize error code");
    write_msg(write, TAG_ERROR, &payload).await
}

/// Does an `ERROR` payload's code end the whole session (`TX_STALE` /
/// `PEER_GONE`), as opposed to just the request? The network relay forwards
/// every `ERROR` verbatim but must stop the pipe on a session-ending one.
pub(crate) fn error_ends_session(payload: &[u8]) -> bool {
    serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|v| v["code"].as_str().map(str::to_string))
        .is_some_and(|code| code == "TX_STALE" || code == "PEER_GONE")
}

/// Writes a message: `u32` length (tag + payload), the tag, the payload, flush.
pub(crate) async fn write_msg<W: AsyncWrite + Unpin>(
    write: &mut W,
    tag: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let len = u32::try_from(1 + payload.len()).map_err(|_| unexpected("message too large"))?;
    bounded(write.write_all(&len.to_be_bytes())).await?;
    bounded(write.write_all(&[tag])).await?;
    bounded(write.write_all(payload)).await?;
    bounded(write.flush()).await
}

/// Reads a message: `Ok(None)` on a clean EOF between messages. The announced
/// length is bounded BEFORE any allocation — a peer does not choose our memory.
pub(crate) async fn read_msg<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_MSG {
        return Err(unexpected("bad message length"));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let tag = buf[0];
    buf.remove(0);
    Ok(Some((tag, buf)))
}

/// Applies the no-progress budget to an I/O future.
pub(crate) async fn bounded<T>(fut: impl Future<Output = std::io::Result<T>>) -> std::io::Result<T> {
    match tokio::time::timeout(STALL, fut).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "data channel: no progress",
        )),
    }
}

pub(crate) fn unexpected(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string())
}
