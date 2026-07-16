// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Data channels: the second connection that carries payloads (inline clipboard
//! blobs, file ranges), so a heavy paste never delays the control plane. A
//! component obtains a single-use `channel_token` over the control plane
//! (`transactions.open` mints a consumer token; `clipboard.get_data` carries a
//! provider token), opens a fresh connection here, sends one LSP-framed attach
//! frame `{"channel_token": …}`, and the connection becomes the binary
//! protocol frozen in `core/src/datachannel.rs`.
//!
//! Two directions, one per type:
//! - [`ConsumerChannel`] (destination side): the component DRIVES — one request
//!   in flight, `FETCH`/`READ`, answered by `DATA*` then `EOF`, or an `ERROR`.
//! - [`ProviderChannel`] (source side): the backend PUSHES the requested inline
//!   blob — `DATA*` then `EOF`, or `ERROR`.
//!
//! Framing (after the attach frame): `[u32 BE length L][1 tag byte][L-1 payload]`
//! with `L = 1 + payload.len()`; a `DATA` payload is `[u64 BE offset][bytes]`.

use std::path::Path;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::framing;
use crate::transport::{self, Stream};

// Tag constants — mirror of `core/src/datachannel.rs`. A second copy of the
// grammar, like the LSP framing: kept in step by the shared integration tests
// against the real Core.
const TAG_READ: u8 = 0x01;
const TAG_FETCH: u8 = 0x02;
const TAG_DATA: u8 = 0x10;
const TAG_EOF: u8 = 0x11;
const TAG_ERROR: u8 = 0x12;

/// Ceiling on a single received message (the Core's `MAX_MSG`): 1 MiB of
/// payload plus framing slack. A `DATA` chunk never exceeds it; we bound the
/// announced length before allocating (the Core is semi-trusted, not blindly).
const MAX_MSG: usize = 1024 * 1024 + 16;
/// Chunk a provider blob into frames comfortably below `MAX_MSG`.
const CHUNK: usize = 64 * 1024;
/// Parsed frames buffered between the consumer's reader task and its methods.
const FRAME_CAPACITY: usize = 8;
/// Ceiling on a whole inline blob accumulated by `fetch` — text/images ride
/// "in RAM like local" (uncapped by contract for legitimate content), but a
/// misbehaving peer must not choose our allocation: an inline pull beyond this
/// fails instead of growing until OOM. Generous — far above any real clipboard.
const MAX_INLINE_FETCH: usize = 256 * 1024 * 1024;

/// A data-channel error code, from an `ERROR` frame or a channel close.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    /// The transaction is gone (superseded, or the source logged out / stopped)
    /// — **terminal**: the session is over, the channel is closed.
    TxStale,
    /// The OS clipboard moved on since the announce (inline only) — request
    /// scoped.
    ClipStale,
    /// A manifest file changed under the frozen identity — request scoped.
    FileChanged,
    /// No such `file_id` in the manifest, or it is a directory — request scoped.
    FileUnknown,
    /// The transaction does not offer that inline format — request scoped.
    FormatUnknown,
    /// The remote source vanished mid-stream — **terminal**.
    PeerGone,
    /// The Core's stall budget elapsed — request scoped.
    Timeout,
    /// An unrecognized (or absent) code — forward-compatible.
    Other(String),
}

impl ErrorCode {
    fn parse(payload: &[u8]) -> ErrorCode {
        let code = serde_json::from_slice::<Value>(payload)
            .ok()
            .and_then(|v| v["code"].as_str().map(str::to_string));
        match code.as_deref() {
            Some("TX_STALE") => ErrorCode::TxStale,
            Some("CLIP_STALE") => ErrorCode::ClipStale,
            Some("FILE_CHANGED") => ErrorCode::FileChanged,
            Some("FILE_UNKNOWN") => ErrorCode::FileUnknown,
            Some("FORMAT_UNKNOWN") => ErrorCode::FormatUnknown,
            Some("PEER_GONE") => ErrorCode::PeerGone,
            Some("TIMEOUT") => ErrorCode::Timeout,
            Some(other) => ErrorCode::Other(other.to_string()),
            None => ErrorCode::Other(String::new()),
        }
    }

    /// Whether this code ends the whole session (the Core closes the channel):
    /// `TX_STALE` and `PEER_GONE`. Everything else is request-scoped — the
    /// channel stays usable. Mirror of the Core's `error_ends_session`.
    pub fn is_terminal(&self) -> bool {
        matches!(self, ErrorCode::TxStale | ErrorCode::PeerGone)
    }
}

/// What a consumer request or a channel-lifetime watch can end with.
#[derive(Debug)]
pub enum ChannelError {
    /// The Core answered `ERROR { code }`.
    Code(ErrorCode),
    /// The channel closed without answering (a dead Core, or a session end
    /// with no explicit terminal error).
    Closed,
    /// An I/O or framing failure on the channel.
    Io(std::io::Error),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::Code(c) => write!(f, "data channel error: {c:?}"),
            ChannelError::Closed => write!(f, "data channel closed"),
            ChannelError::Io(e) => write!(f, "data channel I/O: {e}"),
        }
    }
}

impl std::error::Error for ChannelError {}

// ---------------------------------------------------------------------------
// Consumer channel (destination side): the component drives.
// ---------------------------------------------------------------------------

/// A paste session's read side. Open one per concurrent request the OS needs;
/// the contract is **one request at a time, run to completion** — the Core
/// services each request fully (no mid-request cancellation on the wire). To
/// abandon a paste, drop the channel: `Drop` tears down the connection at once
/// (the Core sees EOF and ends the session), rather than lingering to the
/// Core's stall timeout.
///
/// The socket read half is owned by an internal task that funnels parsed frames
/// through a bounded channel. Two consequences: [`ConsumerChannel::ended`] can
/// be raced in a `select!` against OS events without losing a half-read frame
/// (the frame codec's `read_exact` is not cancel-safe; an `mpsc::recv` is); and
/// if a `read`/`fetch` future is dropped mid-response (a lost `select!` race),
/// the abandoned response's trailing frames are transparently drained by the
/// next `read`/`fetch` — so a cancelled request never desynchronizes the
/// channel into returning another request's bytes.
pub struct ConsumerChannel {
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    frames: mpsc::Receiver<std::io::Result<(u8, Vec<u8>)>>,
    reader: tokio::task::JoinHandle<()>,
    /// A request has been written but its response not yet fully consumed. Set
    /// before the request is sent and cleared only when `collect` returns; if a
    /// `read`/`fetch` future is dropped in between, it stays set and the next
    /// request drains the abandoned response first.
    draining: bool,
}

impl Drop for ConsumerChannel {
    fn drop(&mut self) {
        // Abort the reader task so its read half drops; together with the write
        // half (dropped with the struct) the underlying stream closes and the
        // Core sees EOF immediately — no lingering session to the stall timeout.
        self.reader.abort();
    }
}

impl ConsumerChannel {
    /// Opens the channel and attaches `channel_token` (minted by
    /// `transactions.open`). Fails only on connect / attach I/O.
    pub async fn open(ipc_path: &Path, channel_token: &str) -> std::io::Result<ConsumerChannel> {
        let stream = transport::connect(ipc_path).await?;
        let (read, mut writer) = tokio::io::split(stream);
        attach(&mut writer, channel_token).await?;
        let (tx, frames) = mpsc::channel(FRAME_CAPACITY);
        let reader = tokio::spawn(read_loop(BufReader::new(read), tx));
        Ok(ConsumerChannel {
            writer: Box::new(writer),
            frames,
            reader,
            draining: false,
        })
    }

    /// Reads `len` bytes at `offset` of manifest file `file_id`. Returns the
    /// intersection with the file (possibly fewer bytes at EOF); more than
    /// `len` bytes from a misbehaving peer is a protocol error.
    pub async fn read(
        &mut self,
        file_id: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ChannelError> {
        let payload = json!({ "file_id": file_id, "offset": offset, "len": len });
        let limit = usize::try_from(len).unwrap_or(usize::MAX);
        self.request(TAG_READ, payload, limit).await
    }

    /// Fetches a whole inline blob (`text`, `image/png`), bounded by
    /// [`MAX_INLINE_FETCH`].
    pub async fn fetch(&mut self, format: &str) -> Result<Vec<u8>, ChannelError> {
        self.request(TAG_FETCH, json!({ "format": format }), MAX_INLINE_FETCH)
            .await
    }

    /// Between requests: resolves when the Core pushes a terminal error
    /// (`TX_STALE` on a source logout/stop, a supersession reset) or closes the
    /// channel. Cancel-safe — race it against OS paste events in a `select!`.
    /// Await it only when no request is in flight.
    pub async fn ended(&mut self) -> ChannelError {
        loop {
            match self.frames.recv().await {
                None => return ChannelError::Closed,
                Some(Ok((TAG_ERROR, payload))) => {
                    return ChannelError::Code(ErrorCode::parse(&payload));
                }
                // A stray non-error frame with no request pending: ignore.
                Some(Ok(_)) => {}
                Some(Err(e)) => return ChannelError::Io(e),
            }
        }
    }

    async fn request(
        &mut self,
        tag: u8,
        payload: Value,
        limit: usize,
    ) -> Result<Vec<u8>, ChannelError> {
        // A prior request was dropped mid-response: consume its leftover frames
        // before issuing this one, so we never read its bytes as ours.
        if self.draining {
            self.drain_to_eof().await?;
        }
        let bytes = serde_json::to_vec(&payload).expect("json serialization");
        // Set before the write: if this future is dropped anywhere from here
        // on, `draining` stays set and the next request resynchronizes.
        self.draining = true;
        write_msg(&mut self.writer, tag, &bytes)
            .await
            .map_err(ChannelError::Io)?;
        let result = self.collect(limit).await;
        // collect returned → a response boundary (EOF or ERROR) was reached, or
        // the channel is dead: either way nothing of this request is left.
        self.draining = false;
        result
    }

    async fn collect(&mut self, limit: usize) -> Result<Vec<u8>, ChannelError> {
        let mut buf = Vec::new();
        loop {
            match self.frames.recv().await {
                None => return Err(ChannelError::Closed),
                Some(Err(e)) => return Err(ChannelError::Io(e)),
                Some(Ok((TAG_DATA, payload))) => {
                    if payload.len() < 8 {
                        return Err(ChannelError::Io(invalid(
                            "DATA frame without offset header",
                        )));
                    }
                    if buf.len() + (payload.len() - 8) > limit {
                        return Err(ChannelError::Io(invalid(
                            "data-channel response exceeds its limit",
                        )));
                    }
                    buf.extend_from_slice(&payload[8..]);
                }
                Some(Ok((TAG_EOF, _))) => return Ok(buf),
                Some(Ok((TAG_ERROR, payload))) => {
                    return Err(ChannelError::Code(ErrorCode::parse(&payload)));
                }
                Some(Ok((tag, _))) => {
                    return Err(ChannelError::Io(invalid(&format!(
                        "unexpected data-channel tag {tag:#x}"
                    ))));
                }
            }
        }
    }

    /// Consumes an abandoned response's remaining frames up to its `EOF`/`ERROR`
    /// (or a channel close), discarding the bytes, to realign the stream.
    async fn drain_to_eof(&mut self) -> Result<(), ChannelError> {
        loop {
            match self.frames.recv().await {
                None => return Err(ChannelError::Closed),
                Some(Err(e)) => return Err(ChannelError::Io(e)),
                Some(Ok((TAG_EOF, _))) | Some(Ok((TAG_ERROR, _))) => {
                    self.draining = false;
                    return Ok(());
                }
                // DATA (or anything else): part of the abandoned response.
                Some(Ok(_)) => {}
            }
        }
    }
}

/// Reads frames until EOF or error, forwarding each to `tx`. The bounded
/// channel is the backpressure seam: a consumer that stops draining suspends
/// this task, which stops reading the socket, which the Core throttles.
async fn read_loop<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    tx: mpsc::Sender<std::io::Result<(u8, Vec<u8>)>>,
) {
    loop {
        match read_msg(&mut reader).await {
            Ok(None) => return, // clean close between frames
            Ok(Some(frame)) => {
                if tx.send(Ok(frame)).await.is_err() {
                    return; // channel dropped
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Provider channel (source side): the backend pushes.
// ---------------------------------------------------------------------------

/// The source side of an inline pull: after receiving `clipboard.get_data`, the
/// backend opens this with the request's `channel_token`, streams the blob, and
/// only then replies to the RPC (the reply is the completion signal). Push-only
/// — the Core never sends on a provider channel.
pub struct ProviderChannel {
    stream: Stream,
}

impl ProviderChannel {
    /// Opens the channel and attaches the provider `channel_token` carried by
    /// `clipboard.get_data`.
    pub async fn open(ipc_path: &Path, channel_token: &str) -> std::io::Result<ProviderChannel> {
        let mut stream = transport::connect(ipc_path).await?;
        attach(&mut stream, channel_token).await?;
        Ok(ProviderChannel { stream })
    }

    /// Writes `bytes` starting at `offset`, chunked below the frame ceiling.
    pub async fn data(&mut self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        for (i, chunk) in bytes.chunks(CHUNK).enumerate() {
            write_data(&mut self.stream, offset + (i * CHUNK) as u64, chunk).await?;
        }
        Ok(())
    }

    /// Ends the blob successfully. The `clipboard.get_data` reply must follow.
    pub async fn eof(mut self) -> std::io::Result<()> {
        write_msg(&mut self.stream, TAG_EOF, &[]).await?;
        self.stream.shutdown().await
    }

    /// Ends the blob with an error (`CLIP_STALE` when the OS clipboard moved on).
    pub async fn error(mut self, code: &str) -> std::io::Result<()> {
        let payload = serde_json::to_vec(&json!({ "code": code })).expect("json serialization");
        write_msg(&mut self.stream, TAG_ERROR, &payload).await?;
        self.stream.shutdown().await
    }
}

// ---------------------------------------------------------------------------
// Wire codec (byte-exact mirror of core/src/datachannel.rs).
// ---------------------------------------------------------------------------

/// Sends the single LSP-framed attach frame that turns a fresh connection into
/// a data channel.
async fn attach<W: AsyncWrite + Unpin>(writer: &mut W, channel_token: &str) -> std::io::Result<()> {
    let frame = framing::encode(&json!({ "channel_token": channel_token }).to_string());
    writer.write_all(&frame).await?;
    writer.flush().await
}

async fn write_msg<W: AsyncWrite + Unpin>(
    writer: &mut W,
    tag: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let len =
        u32::try_from(1 + payload.len()).map_err(|_| invalid("data-channel frame too large"))?;
    // One buffer, one write_all: a cancellation cannot tear the frame across
    // separate writes (the length prefix, the tag, and the payload always go
    // out together or not at all).
    let mut frame = Vec::with_capacity(4 + 1 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.push(tag);
    frame.extend_from_slice(payload);
    writer.write_all(&frame).await?;
    writer.flush().await
}

async fn write_data<W: AsyncWrite + Unpin>(
    writer: &mut W,
    offset: u64,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut payload = Vec::with_capacity(8 + bytes.len());
    payload.extend_from_slice(&offset.to_be_bytes());
    payload.extend_from_slice(bytes);
    write_msg(writer, TAG_DATA, &payload).await
}

/// `Ok(None)` = clean EOF between frames. Bounds the announced length before
/// allocating.
async fn read_msg<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_MSG {
        return Err(invalid("data-channel frame length out of bounds"));
    }
    let mut frame = vec![0u8; len];
    reader.read_exact(&mut frame).await?;
    let tag = frame[0];
    frame.remove(0);
    Ok(Some((tag, frame)))
}

fn invalid(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string())
}
