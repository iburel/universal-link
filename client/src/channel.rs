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
//!
//! A third type rides the very same second connection with another grammar:
//! [`PeerChannel`] (`peers.channel`, #124), a live duplex pipe between the
//! components holding the same role on two devices. Its frames carry no tag and
//! no offset, only a length and opaque bytes, because the Core interprets none
//! of them. It is grouped here rather than in a module of its own for one
//! reason: everything a second connection has in common (the dial, the single
//! LSP-framed attach frame `attach` writes, the reader task that makes reads
//! cancel-safe) is then written once, for all three types.

use std::path::Path;
use std::time::Duration;

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
    /// The deployment's relays are rendezvous-only above a cap (#88) and no
    /// direct path to the source formed. **Terminal**: the Core refused to
    /// open the relay-borne session at all, so the channel closes right
    /// after this frame. The remedy is the pair's (a shared network, a
    /// VPN), not a retry on the same route.
    NoDirectPath,
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
            Some("NO_DIRECT_PATH") => ErrorCode::NoDirectPath,
            Some("TIMEOUT") => ErrorCode::Timeout,
            Some(other) => ErrorCode::Other(other.to_string()),
            None => ErrorCode::Other(String::new()),
        }
    }

    /// Whether this code ends the whole session (the Core closes the channel):
    /// `TX_STALE`, `PEER_GONE` and `NO_DIRECT_PATH`. Everything else is
    /// request-scoped, the channel stays usable. Mirror of the Core's
    /// `error_ends_session`, plus the sized-open refusal (#88), which the
    /// destination Core writes as its one and only frame before hanging up.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ErrorCode::TxStale | ErrorCode::PeerGone | ErrorCode::NoDirectPath
        )
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

// ---------------------------------------------------------------------------
// Peer channel (#124): an opaque duplex pipe between the same role on two
// devices. The grammar lives HERE and nowhere else in this crate: the input
// component (#122) consumes this type, and a private copy of the codec in a
// component is how the two ends of a frozen framing drift apart.
// ---------------------------------------------------------------------------

/// Ceiling on ONE peer-channel frame, in either direction: the Core's
/// `MAX_FRAME` (`core/src/peerchannel.rs`, 1 KiB, the size #123 measured the
/// input flow needs).
///
/// The two values MUST agree, and if the Core's ever moves this constant moves
/// with it in the same commit. [`PeerChannel::send`] refuses anything above it
/// LOCALLY, before a byte reaches the socket, because a frame over the cap makes
/// the Core cut the whole channel with `FRAME_TOO_LARGE`: a client that shoots
/// its own pipe dead because a caller asked it to send one oversized message is
/// a bug factory. Refused here, the caller gets a distinct error and the channel
/// stays usable.
pub const MAX_PEER_FRAME: usize = 1024;

/// The narrowest `ClientConfig::request_timeout` a component that calls
/// `peers.channel` may configure, and the value its own tests use.
///
/// The Core does not answer that call until the far end has ATTACHED, which is
/// what makes the token it returns a pipe that is already alive at both ends,
/// and its budgets for getting there are up to 15 s of dialling (a cold relay
/// connect plus the hole-punching grace the rendezvous-only refusal needs in
/// order to be able to speak at all) plus 10 s for the two frames and the far
/// end's attach. A tighter budget on the caller's side turns the one refusal
/// this primitive owes the user (`NO_DIRECT_PATH`, whose remedy is the pair's
/// network) into a bare [`crate::RequestError::Timeout`], which is a lie about
/// whose fault it is. `doc/core-api.md` ("`peers.channel`: a live pipe, for a
/// flow") states the duty; this constant is that duty in code.
pub const PEER_CHANNEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

/// Frames buffered between a peer channel's reader task and
/// [`PeerChannel::recv`].
///
/// 64, chosen against the flow this primitive exists for: 125 Hz to 1000 Hz of
/// tiny frames (#123). Too small (1, say) and every frame costs a full round of
/// task wakeups, because the reader task blocks on the handoff until the
/// consumer takes it, up to a thousand times a second. Too large and a consumer
/// that has stopped draining becomes INVISIBLE: the frames pile up here, the
/// socket keeps being read, and when the consumer comes back it works through
/// positions from the past instead of the present, which for input is worse than
/// not working through them at all. 64 is 64 ms of a 1000 Hz flow (an eternity
/// for a consumer merely between polls, nothing at all against the Core's 10 s
/// idle sweep) and it bounds this buffer at 64 KiB in the worst case of frames
/// at the cap, where a real input flow sits nearer 1 KiB.
const PEER_FRAME_CAPACITY: usize = 64;

/// What sending on a peer channel can fail with.
#[derive(Debug)]
pub enum PeerSendError {
    /// The frame is above [`MAX_PEER_FRAME`], and was refused HERE: nothing was
    /// written, the channel is untouched and stays usable. Always a caller bug
    /// (a dialect that outgrew the cap), never a peer's and never a network's.
    TooLarge {
        /// Length of the refused frame, for the caller's log.
        len: usize,
    },
    /// The pipe is gone: the Core closed it, or the connection broke. WHY it
    /// ended is not in here and cannot be: it arrives as `peer.channel_closed`
    /// on the control connection (see [`PeerChannel`]).
    Io(std::io::Error),
}

impl std::fmt::Display for PeerSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerSendError::TooLarge { len } => write!(
                f,
                "peer-channel frame of {len} bytes above the {MAX_PEER_FRAME}-byte cap"
            ),
            PeerSendError::Io(e) => write!(f, "peer channel I/O: {e}"),
        }
    }
}

impl std::error::Error for PeerSendError {}

/// A live duplex pipe to the components holding the SAME role on another device
/// of the account (`peers.channel`, #124). The Core relays opaque bytes and
/// interprets none of them: the dialect is the two components', and what this
/// type owns is the frame, the cap, and the moment the pipe ends.
///
/// ONE type for both ends, on purpose. A channel this device opened (the token
/// comes back in the `peers.channel` reply) and one a peer opened (the token
/// arrives as a `peer.channel` notification) differ in nothing once attached:
/// the pipe is duplex and neither end is the driver, so two types would be two
/// names for one thing.
///
/// # The reason a channel ended NEVER travels on the pipe
///
/// The most important thing a caller can get wrong. A `PeerChannel` that just
/// ended says only THAT it ended: [`PeerChannel::recv`] returns `None` and a
/// [`PeerChannel::send`] fails with [`PeerSendError::Io`]. The reason arrives as
/// a `peer.channel_closed { device_id, reason }` notification on the CONTROL
/// connection that owned the local end, with one of `CLOSED`, `REPLACED`,
/// `PEER_GONE`, `DEVICE_REVOKED`, `LOGGED_OUT`, `ACCOUNT_LEFT`, `SHUTDOWN`,
/// `FRAME_TOO_LARGE`, `RATE_EXCEEDED`, `IDLE_TIMEOUT`, `NO_DIRECT_PATH`. It
/// cannot ride the pipe: a closed stream carries no reason, and no control frame
/// lives inside a grammar the Core refuses to look inside. A component that
/// watches only its pipe can report "it stopped" and never why, so watch both.
///
/// # Liveness is the caller's duty
///
/// The Core sweeps a channel with no frame in EITHER direction for 10 s
/// (`IDLE_TIMEOUT`). Any frame resets it, a zero-length one included, so a
/// keepalive costs 4 bytes at whatever cadence the component picks. This type
/// sends none by itself: the cadence is the component's policy, and a client
/// that silently kept a dead peer's channel alive would hide exactly what the
/// sweep exists to reveal.
///
/// # Shape
///
/// The read half is owned by an internal task funnelling parsed frames through a
/// bounded channel, like [`ConsumerChannel`] and for the same two reasons:
/// [`PeerChannel::recv`] must be cancel-safe so it can be raced in a `select!`
/// against the control plane's events (the frame codec's `read_exact` is not
/// cancel-safe, an `mpsc::recv` is), and the bounded channel is the backpressure
/// seam (`PEER_FRAME_CAPACITY`, whose size is argued where it is defined).
///
/// No convenience wraps "wait for a frame, and learn if the channel died"
/// deliberately: [`PeerChannel::recv`] IS that one call, and a sibling that
/// resolved only on the end would invite a caller to await both at once and lose
/// a frame to whichever won.
pub struct PeerChannel {
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    frames: mpsc::Receiver<std::io::Result<Vec<u8>>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for PeerChannel {
    fn drop(&mut self) {
        // Abort the reader task so its read half drops; with the write half
        // (dropped along with the struct) the stream closes, and the Core ends
        // the channel at once: `CLOSED` here, `PEER_GONE` at the far end, rather
        // than a live channel nobody reads waiting out a Core-side budget.
        self.reader.abort();
    }
}

impl PeerChannel {
    /// Opens the pipe and attaches `channel_token`. Serves both ends
    /// identically: a token from a `peers.channel` reply, or one from a
    /// `peer.channel` notification.
    ///
    /// Fails only on connect / attach I/O. A token the Core does not honor (spent,
    /// unknown, or belonging to a channel whose trust ended in the meantime) is
    /// NOT an error here: the connection is accepted and then closed, which shows
    /// up as [`PeerChannel::recv`] returning `None` right away, with the reason on
    /// the control plane like any other end.
    ///
    /// A caller minting the token with `peers.channel` must give that request at
    /// least [`PEER_CHANNEL_REQUEST_TIMEOUT`].
    pub async fn open(ipc_path: &Path, channel_token: &str) -> std::io::Result<PeerChannel> {
        let stream = transport::connect(ipc_path).await?;
        let (read, mut writer) = tokio::io::split(stream);
        attach(&mut writer, channel_token).await?;
        let (tx, frames) = mpsc::channel(PEER_FRAME_CAPACITY);
        let reader = tokio::spawn(peer_read_loop(BufReader::new(read), tx));
        Ok(PeerChannel {
            writer: Box::new(writer),
            frames,
            reader,
        })
    }

    /// Sends one frame. `bytes` empty is legal and is the keepalive: to a Core
    /// that does not know what a keepalive is, that is all one can look like.
    ///
    /// Above [`MAX_PEER_FRAME`] the frame is refused without being written
    /// ([`PeerSendError::TooLarge`]) and the channel stays usable.
    pub async fn send(&mut self, bytes: &[u8]) -> Result<(), PeerSendError> {
        if bytes.len() > MAX_PEER_FRAME {
            return Err(PeerSendError::TooLarge { len: bytes.len() });
        }
        write_peer_frame(&mut self.writer, bytes)
            .await
            .map_err(PeerSendError::Io)
    }

    /// The next frame. `None` = the pipe ENDED (cleanly or not: the reason is on
    /// the control plane, see the type's documentation); `Some(Err(..))` = an I/O
    /// or framing failure that is this side's business, i.e. a frame announced
    /// above the cap, which is a Core or a peer not speaking this primitive.
    ///
    /// Cancel-safe: a `recv` dropped in a lost `select!` race loses no frame.
    pub async fn recv(&mut self) -> Option<Result<Vec<u8>, std::io::Error>> {
        self.frames.recv().await
    }
}

/// Reads frames until the pipe ends or breaks, forwarding each to `tx`. The
/// bounded channel is the backpressure seam: a consumer that stops draining
/// suspends this task, which stops reading the socket, which the Core throttles
/// (and, past its idle and rate budgets, cuts).
async fn peer_read_loop<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    tx: mpsc::Sender<std::io::Result<Vec<u8>>>,
) {
    loop {
        match read_peer_frame(&mut reader).await {
            Ok(None) => return, // the pipe ended
            Ok(Some(frame)) => {
                if tx.send(Ok(frame)).await.is_err() {
                    return; // the PeerChannel was dropped
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return;
            }
        }
    }
}

/// `[u32 BE length][exactly that many bytes]`, one buffer and one `write_all`.
/// The Core writes the two halves separately (measured as free in #123), but on
/// this side a single write is what keeps a cancellation from tearing a frame in
/// two: the length and the body always go out together or not at all.
async fn write_peer_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> std::io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| invalid("peer-channel frame too large"))?;
    let mut frame = Vec::with_capacity(4 + bytes.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(bytes);
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// `Ok(None)` = the pipe ended. Bounds the announced length against the cap
/// BEFORE allocating: the Core is semi-trusted, and the far end is a peer.
async fn read_peer_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if is_end_of_pipe(&e) => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    // A zero length is a legal frame (the keepalive); above the cap is not a
    // frame at all, and we say so instead of allocating what was announced.
    if len > MAX_PEER_FRAME {
        return Err(invalid("peer-channel frame above the cap"));
    }
    let mut frame = vec![0u8; len];
    match reader.read_exact(&mut frame).await {
        Ok(_) => Ok(Some(frame)),
        // An end of stream in the MIDDLE of a frame is a close, not a violation:
        // the length and the body are two writes on the Core's side, so a
        // channel cut between them legitimately looks exactly like this.
        Err(e) if is_end_of_pipe(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Does this read error mean "the pipe ended" rather than "something failed"? A
/// closed Unix socket reads as `UnexpectedEof` (or `ConnectionReset` when it was
/// reset), a closed Windows named pipe as `BrokenPipe`. The three collapse
/// because on this type they carry the same amount of information: none. What
/// ended the channel is a `peer.channel_closed` reason on the control plane, and
/// a caller that reads it there is told the same thing whichever of these the
/// socket happened to produce.
fn is_end_of_pipe(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
    )
}

fn invalid(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string())
}
