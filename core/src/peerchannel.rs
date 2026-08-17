// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! A live channel between the components holding the SAME role on two devices
//! of the account (doc/core-api.md, "peers.*"; ticket #124). What `peers.send`
//! is for a gesture, this is for a flow: `peers.send` opens a stream, writes
//! one message and awaits an ack, which is exactly right for announcing a
//! layout and unusable for a stream of pointer positions.
//!
//! The Core carries OPAQUE bytes and interprets none of them. It knows nothing
//! about keyboards, screens or grants: the dialect is the components', and this
//! module's whole subject is the pipe, its caps, and the moment it must die.
//!
//! # The shape
//!
//! `peers.channel { device_id }` gives `{ channel_token }`: single-use,
//! unguessable, bound to one component, one peer and one direction of opening,
//! exactly like the data channel's token. The bearer opens a SECOND connection
//! to the socket, presents it, and that connection becomes an opaque framed
//! pipe. On the target device the Core delivers `peer.channel { device_id,
//! channel_token }` to the components holding the sender's role AND the
//! `peers.channel` scope: no topic, no subscription, the scope IS the
//! subscription, which is `peer.message`'s doctrine.
//!
//! # Framing (frozen here)
//!
//! `u32` big-endian length, then exactly that many bytes, on BOTH halves: the
//! local IPC connection and the peer stream speak the identical grammar, so the
//! pipe is a byte-for-byte relay and the Core never has to look inside a frame.
//! A frame is capped at [`MAX_FRAME`] (1 KiB, #123's number for the input use)
//! and a zero-length frame is legal, which is what a component's keepalive
//! looks like to a Core that does not know what a keepalive is.
//!
//! Two-write framing on purpose (`dataplane::write_frame`): #123 measured that
//! composing the length into a single write saved 0.067 ms of round trip on the
//! same machine and nothing at all across a network, so the data plane's
//! existing writer stays.
//!
//! # Why an opaque second connection at all
//!
//! Not for speed: #123 measured ordinary control-plane notifications through a
//! real Core at 0.225 ms one way at 125 Hz, p99 0.552, with JSON costing
//! nothing at this size. The pipe buys under 0.05 ms. It is kept for
//! ISOLATION and BACKPRESSURE: input frames never touch the JSON-RPC layer, a
//! 125 Hz flow never shares a queue with an approval prompt, and a pipe has
//! natural flow control where a bounded notification queue has only the
//! fail-closed close.
//!
//! # What it deliberately is not
//!
//! No store and forward (an offline peer simply cannot be opened to), no
//! reconnection logic (the component retries on its own schedule, and the
//! `devices` topic already says when a peer is reachable again), no
//! interpretation, and no logging of payloads.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};

use crate::connector::IoStream;
use crate::dataplane;
use crate::rpc::RpcErr;
use crate::state::{AppState, ChannelGrant, ChannelKind, ConnId};

/// The scope that guards the primitive, grantable to any component like
/// `peers.message`: it opens channels AND it is the subscription that receives
/// them.
pub(crate) const SCOPE: &str = "peers.channel";

/// Cap on one frame, in either direction (#123: an input event is about a
/// dozen bytes, and frame size is free at these sizes on every path measured,
/// so the cap is set by what the use needs and not by what the transport
/// tolerates). A frame above it is a component or a peer that is not speaking
/// this primitive: the channel dies rather than the Core growing its buffer.
pub(crate) const MAX_FRAME: usize = 1024;

/// Frames allowed per second, in either direction, over a fixed window. The
/// highest rate this primitive was designed for is a 1000 Hz gaming mouse
/// (measured in #123), so four times that is headroom no legitimate flow
/// reaches and a runaway component cannot hide behind. A fixed window admits
/// twice the cap across a window boundary; that is a deliberate simplification,
/// the cap is a guardrail and not a scheduler.
const RATE_MAX_PER_WINDOW: u32 = 4000;
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// No frame in EITHER direction for this long and the channel is swept. A
/// component that wants a long silent channel sends a frame: the Core does not
/// know what a keepalive is, so any frame, a zero-length one included, counts.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Whole-exchange budget for `peers.channel` (connect + one frame each way +
/// the far end's attach): the house exchange norm, as `peers.send` uses.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a minted `channel_token` waits for its second connection. On the
/// receiving side it is also how long the far Core holds its answer, so it must
/// stay comfortably under [`OPEN_TIMEOUT`]: a component that does not come is
/// answered `COMPONENT_ABSENT` rather than left to the caller's timeout.
const ATTACH_GRACE: Duration = Duration::from_secs(5);

/// How often a live channel re-checks that its peer is still an attested
/// device of the account. A poll and not a hook, deliberately: the directory
/// moves through many paths (a local `devices.revoke`, a tombstone absorbed
/// from another device, a fresh server snapshot, a record re-signed under
/// another key), and a channel that outlives trust by half a second is a bug
/// where a channel that misses one of those paths entirely is a hole. It also
/// carries the idle sweep, for the same one-timer reason.
const TRUST_POLL: Duration = Duration::from_millis(500);

/// Why a channel ended, as the `reason` of `peer.channel_closed`. Codes, not
/// sentences: the interface owns the wording, like every code in this API.
pub(crate) mod reason {
    /// The local component closed its end (or its process died).
    pub(crate) const CLOSED: &str = "CLOSED";
    /// A newer channel on the same (role, device) pair took its place.
    pub(crate) const REPLACED: &str = "REPLACED";
    /// The far end ended the stream: its component left, its Core stopped, it
    /// struck this device off, or the path broke.
    pub(crate) const PEER_GONE: &str = "PEER_GONE";
    /// The peer is no longer an attested device of the account here: struck
    /// off, record gone, or an attestation that stopped verifying. The three
    /// collapse on purpose, exactly as they do behind `DEVICE_UNKNOWN` at the
    /// door: fail-closed, and no diagnosis owed.
    pub(crate) const DEVICE_REVOKED: &str = "DEVICE_REVOKED";
    /// `session.logout`: the account's grants do not outlive its session.
    pub(crate) const LOGGED_OUT: &str = "LOGGED_OUT";
    /// This device was struck off the account it was speaking for.
    pub(crate) const ACCOUNT_LEFT: &str = "ACCOUNT_LEFT";
    /// The Core is stopping.
    pub(crate) const SHUTDOWN: &str = "SHUTDOWN";
    /// A frame above the 1 KiB cap, in either direction.
    pub(crate) const FRAME_TOO_LARGE: &str = "FRAME_TOO_LARGE";
    /// The frame-rate cap was exceeded, in either direction.
    pub(crate) const RATE_EXCEEDED: &str = "RATE_EXCEEDED";
    /// No frame in either direction within the idle budget.
    pub(crate) const IDLE_TIMEOUT: &str = "IDLE_TIMEOUT";
}

// ---------------------------------------------------------------------------
// The live table: one channel per (role, peer) pair.
// ---------------------------------------------------------------------------

/// Every channel currently piping, keyed by `(role, device_id)`. ONE per pair:
/// a second open on the same pair replaces the first rather than multiplying
/// them, which is what keeps a component that retries after a hiccup from
/// leaking pipes. The key ignores WHICH side opened, because the pipe is
/// duplex and a second one in the other direction would carry nothing the
/// first cannot: if both ends open at once, the later open wins on both devices
/// and the earlier is closed with `REPLACED`.
///
/// LEAF lock: taken alone, never across an await, never while holding
/// `registry` or `session`.
#[derive(Default)]
pub struct PeerChannels {
    live: std::collections::BTreeMap<(String, String), Live>,
    next_id: u64,
}

struct Live {
    /// Identifies THIS channel among the ones that held the key. A channel that
    /// has already been replaced must not unregister its successor on the way
    /// out, so every removal compares the ordinal.
    id: u64,
    cut: Arc<Cut>,
}

/// The one-shot order to stop a pipe, and the reason to report. `notify_one`
/// (not `notify_waiters`): it stores a permit, so an order fired between two
/// turns of the vigil's loop is never lost.
struct Cut {
    reason: Mutex<Option<&'static str>>,
    wake: Notify,
}

impl Cut {
    fn fire(&self, reason: &'static str) {
        let mut slot = self.reason.lock().expect("lock cut reason");
        // First reason wins: a logout landing on a channel already cut by a
        // revocation does not rewrite history.
        if slot.is_none() {
            *slot = Some(reason);
        }
        drop(slot);
        self.wake.notify_one();
    }

    fn reason(&self) -> Option<&'static str> {
        *self.reason.lock().expect("lock cut reason")
    }
}

impl PeerChannels {
    /// Registers a channel that is about to start piping, cutting whatever held
    /// the same pair, and returns the ordinal plus the cut order to obey.
    fn register(&mut self, role: &str, device_id: &str) -> (u64, Arc<Cut>) {
        self.next_id += 1;
        let id = self.next_id;
        let cut = Arc::new(Cut {
            reason: Mutex::new(None),
            wake: Notify::new(),
        });
        let previous = self.live.insert(
            (role.to_string(), device_id.to_string()),
            Live {
                id,
                cut: cut.clone(),
            },
        );
        if let Some(previous) = previous {
            previous.cut.fire(reason::REPLACED);
        }
        (id, cut)
    }

    /// Removes a channel, but only if it is still the one holding the key: a
    /// channel that was replaced finds its successor there and leaves it alone.
    fn unregister(&mut self, role: &str, device_id: &str, id: u64) {
        let key = (role.to_string(), device_id.to_string());
        if self.live.get(&key).is_some_and(|live| live.id == id) {
            self.live.remove(&key);
        }
    }
}

/// Cuts every live channel with one reason: what a logout, a Core stop and
/// being struck off the account all need. Each pipe reports the reason to its
/// own component and unregisters itself; nothing is removed here, so the
/// bookkeeping stays in one place.
pub(crate) fn cut_all(state: &AppState, reason: &'static str) {
    let channels = state.peer_channels.lock().expect("lock peer_channels");
    for live in channels.live.values() {
        live.cut.fire(reason);
    }
}

// ---------------------------------------------------------------------------
// The two shapes a `channel_token` can carry.
// ---------------------------------------------------------------------------

/// The peer stream of a channel THIS device opened, parked between
/// `peers.channel` returning the token and the component's second connection
/// coming to take it. Owned by the grant, so the two ways a token can die (the
/// attach grace, the owner's teardown) both drop the stream and the far end
/// sees a clean end with nothing extra to write.
pub struct Parked {
    stream: Box<dyn IoStream>,
    role: String,
    device_id: String,
    node_id: String,
}

/// A channel a PEER opened, waiting for one of the local candidates to attach.
/// The handler still holds the peer stream (it owes the far end an answer), so
/// here it is the component's halves that travel.
pub struct Joining {
    handoff: mpsc::Sender<Attached>,
}

/// The local halves of an attached component, on their way to the handler that
/// holds the matching peer stream.
struct Attached {
    conn_id: ConnId,
    read: Box<dyn AsyncRead + Send + Unpin>,
    write: Box<dyn AsyncWrite + Send + Unpin>,
}

impl Joining {
    /// Hands the attaching connection's halves to the waiting handler. Nothing
    /// is awaited: the handler is parked on a channel of capacity one, and a
    /// full one means a sibling won the race, in which case these halves drop
    /// and that connection closes.
    pub fn hand_over<R, W>(&self, conn_id: ConnId, read: R, write: W)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let _ = self.handoff.try_send(Attached {
            conn_id,
            read: Box::new(read),
            write: Box::new(write),
        });
    }
}

// ---------------------------------------------------------------------------
// Opening: `peers.channel`.
// ---------------------------------------------------------------------------

/// Opens a channel to the components holding `role` on `device_id` and returns
/// the `channel_token` for the local end.
///
/// The targeting is `files.send`'s doctrine, and the whole handshake happens
/// HERE so that every refusal is an answer to the call rather than a silence
/// the component has to interpret: `DEVICE_UNKNOWN` (absent, or attested under
/// a foreign key: indistinguishable, fail-closed), `DEVICE_OFFLINE` (no route,
/// or nothing usable answered in time), `COMPONENT_ABSENT` (the remote Core's
/// word: nobody there holds the role with the scope, or nobody came to take the
/// channel), `NO_DIRECT_PATH` (the deployment's relays are rendezvous-only
/// above a cap and no direct path formed).
///
/// That last one is why the open goes through `open_for_payload` declaring
/// itself above ANY cap: `open` never consults the announced relay cap (#88),
/// so a channel opened through it would quietly ride the rendezvous-only relays
/// of a deployment that said no to exactly that. A session that names no size
/// declares `u64::MAX`, and the refusal is a sentence an interface can say.
pub(crate) async fn open(
    state: &Arc<AppState>,
    role: &str,
    device_id: &str,
    conn_id: ConnId,
    pid: Option<u32>,
) -> Result<Value, RpcErr> {
    let peer =
        dataplane::resolve_peer(state, device_id).ok_or_else(|| RpcErr::app("DEVICE_UNKNOWN"))?;
    if !dataplane::peer_reachable(state, &peer) {
        return Err(RpcErr::app("DEVICE_OFFLINE"));
    }
    let stream = tokio::time::timeout(OPEN_TIMEOUT, handshake(state, &peer, role))
        .await
        .map_err(|_| RpcErr::app("DEVICE_OFFLINE"))??;
    // The pipe cannot start before its component is here, so the stream waits.
    // Reclaimed by the grace sweep below and by the connection's teardown:
    // either way the grant goes, the stream drops with it, and the far end sees
    // a clean end.
    let parked = Parked {
        stream,
        role: role.to_string(),
        device_id: device_id.to_string(),
        node_id: peer.node_id.clone(),
    };
    let token = state
        .registry
        .lock()
        .expect("lock registry")
        .mint_channel_token(ChannelGrant {
            kind: ChannelKind::PeerOpen(parked),
            pid,
            conn_id,
        });
    let sweep = state.clone();
    let sweep_token = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(ATTACH_GRACE).await;
        sweep
            .registry
            .lock()
            .expect("lock registry")
            .take_channel_token(&sweep_token);
    });
    Ok(json!({ "channel_token": token }))
}

/// Dials the peer and asks for the channel: one `peer_channel` frame out, one
/// `peer_channel_ack` back. Any answer that is not that ack (the tombstone a
/// struck-off dialer is served, a frame from another era) reads as the peer
/// being unreachable for this purpose.
async fn handshake(
    state: &Arc<AppState>,
    peer: &dataplane::PeerAddr,
    role: &str,
) -> Result<Box<dyn IoStream>, RpcErr> {
    let mut stream = state
        .transport
        .open_for_payload(peer, u64::MAX)
        .await
        .map_err(|e| {
            if dataplane::failure_code(&e) == dataplane::NO_DIRECT_PATH {
                RpcErr::app(dataplane::NO_DIRECT_PATH)
            } else {
                RpcErr::app("DEVICE_OFFLINE")
            }
        })?;
    let frame = serde_json::to_vec(&json!({ "type": "peer_channel", "role": role }))
        .expect("serialize peer_channel");
    dataplane::write_frame(&mut stream, &frame)
        .await
        .map_err(|_| RpcErr::app("DEVICE_OFFLINE"))?;
    let reply = dataplane::read_frame(&mut stream)
        .await
        .map_err(|_| RpcErr::app("DEVICE_OFFLINE"))?;
    let reply: Value = serde_json::from_slice(&reply).map_err(|_| RpcErr::app("DEVICE_OFFLINE"))?;
    if reply.get("type").and_then(Value::as_str) != Some("peer_channel_ack") {
        return Err(RpcErr::app("DEVICE_OFFLINE"));
    }
    if reply.get("accepted").and_then(Value::as_bool) != Some(true) {
        return Err(RpcErr::app("COMPONENT_ABSENT"));
    }
    Ok(stream)
}

/// The component came for the channel it asked for: take the parked stream and
/// become the pipe, in this connection's own task. `conn_id` is the CONTROL
/// connection the token was minted for, not this one: a data connection is
/// never registered, and it is the control connection that hears the end.
pub(crate) async fn serve_opened<R, W>(
    state: Arc<AppState>,
    parked: Parked,
    conn_id: ConnId,
    read: R,
    write: W,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    pipe(
        state,
        Endpoints {
            role: parked.role,
            device_id: parked.device_id,
            node_id: parked.node_id,
            conn_id,
        },
        Box::new(read),
        Box::new(write),
        parked.stream,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Receiving: a peer opens a channel to us.
// ---------------------------------------------------------------------------

/// Receiver side of a `peer_channel`: offers the channel to every component
/// holding the sender's role and the scope, waits for one to take it, then
/// answers whether the pipe is joined and pipes it.
///
/// The answer therefore means more than `peer_ack`'s does: `accepted` says the
/// far end is HERE, not merely that somebody was told, so the caller's token
/// opens a channel that is already alive at both ends. The peer is already
/// attested (C7 ran in `serve`); the sender is named by OUR directory, never by
/// its own claim.
pub(crate) async fn recv(
    state: Arc<AppState>,
    peer_node_id: String,
    first: Value,
    mut stream: Box<dyn IoStream>,
) {
    let role = first
        .get("role")
        .and_then(Value::as_str)
        .map(str::to_string);
    // A malformed frame and a role nobody holds collapse into the same answer:
    // the ack owes the sender no inventory of this device's components.
    let Some(role) = role else {
        answer(&mut stream, false).await;
        return;
    };
    let device_id =
        dataplane::device_id_for(&state, &peer_node_id).unwrap_or_else(|| peer_node_id.clone());
    let (handoff, mut arrivals) = mpsc::channel(1);
    // One token per candidate, each bound to its own component: a token is
    // bound to one process, so the fan-out cannot be one broadcast frame the
    // way `peer.message` is. The first to attach wins the pipe.
    let tokens: Vec<String> = {
        let mut reg = state.registry.lock().expect("lock registry");
        let candidates = reg.conns_for_role_scope(&role, SCOPE);
        candidates
            .into_iter()
            .map(|(conn_id, pid)| {
                let token = reg.mint_channel_token(ChannelGrant {
                    kind: ChannelKind::PeerJoin(Joining {
                        handoff: handoff.clone(),
                    }),
                    pid,
                    conn_id,
                });
                reg.notify_conn(
                    conn_id,
                    "peer.channel",
                    &json!({ "device_id": device_id, "channel_token": token }),
                );
                token
            })
            .collect()
    };
    if tokens.is_empty() {
        answer(&mut stream, false).await;
        return;
    }
    // Dropped so the wait ends by itself if every candidate's connection dies
    // before attaching (each grant holds a clone; the last one going closes the
    // channel).
    drop(handoff);
    let arrival = tokio::time::timeout(ATTACH_GRACE, arrivals.recv())
        .await
        .ok()
        .flatten();
    // The single-use tokens are reclaimed whatever happened: the winner's is
    // already gone, its siblings must not linger as a second door into a pipe
    // that is taken.
    {
        let mut reg = state.registry.lock().expect("lock registry");
        for token in &tokens {
            reg.take_channel_token(token);
        }
    }
    let Some(attached) = arrival else {
        answer(&mut stream, false).await;
        return;
    };
    answer(&mut stream, true).await;
    // Detached, and this is the one place it matters: the data plane serves its
    // incoming streams from a bounded set (`MAX_PEER_TASKS`), and a channel that
    // lives for hours would hold one of those slots against every file, paste
    // and directory round the account needs. The pipe's life is governed
    // instead by its own ends: `cut_all` at the Core's stop, the vigil's checks,
    // and either half ending.
    tokio::spawn(pipe(
        state,
        Endpoints {
            role,
            device_id,
            node_id: peer_node_id,
            conn_id: attached.conn_id,
        },
        attached.read,
        attached.write,
        stream,
    ));
}

/// Writes the `peer_channel_ack`. Best-effort: a caller that has already given
/// up is no reason to fail here.
async fn answer(stream: &mut Box<dyn IoStream>, accepted: bool) {
    let ack = serde_json::to_vec(&json!({ "type": "peer_channel_ack", "accepted": accepted }))
        .expect("serialize peer_channel_ack");
    let _ = dataplane::write_frame(stream, &ack).await;
    if !accepted {
        let _ = stream.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// The pipe.
// ---------------------------------------------------------------------------

/// Who this pipe joins: the role it belongs to, the peer as this device's
/// directory names it, and the peer key trust was checked against (a
/// re-enrolled device under a new key is not the device this channel was
/// opened to).
struct Endpoints {
    role: String,
    device_id: String,
    node_id: String,
    conn_id: ConnId,
}

/// Relays frames between a local component and a peer until something ends it,
/// then tells the local component WHICH thing it was. Never a silent stall:
/// every exit here has a reason, and both halves are shut down on the way out
/// so the other end sees a clean end of stream.
async fn pipe(
    state: Arc<AppState>,
    ends: Endpoints,
    mut local_read: Box<dyn AsyncRead + Send + Unpin>,
    mut local_write: Box<dyn AsyncWrite + Send + Unpin>,
    net: Box<dyn IoStream>,
) {
    // The component that owns the channel may have torn down between the mint
    // and this attach; then there is nobody to hand a pipe to, and nobody to
    // tell why it ended. Dropping the stream is the whole answer.
    if !owner_connected(&state, ends.conn_id) {
        return;
    }
    let (id, cut) = {
        let mut channels = state.peer_channels.lock().expect("lock peer_channels");
        channels.register(&ends.role, &ends.device_id)
    };
    let (mut net_read, mut net_write) = tokio::io::split(net);
    // Milliseconds since this pipe started, written by both directions and read
    // by the vigil: the idle budget is about the CHANNEL, not about one
    // direction, so a peer typing while the local end stays silent is not idle.
    let born = Instant::now();
    let last = AtomicU64::new(0);
    let touch = || last.store(born.elapsed().as_millis() as u64, Ordering::Relaxed);

    let up = async {
        let mut rate = Rate::new();
        loop {
            match read_frame(&mut local_read).await {
                Frame::Bytes(bytes) => {
                    touch();
                    if rate.tick() {
                        return reason::RATE_EXCEEDED;
                    }
                    if dataplane::write_frame(&mut net_write, &bytes)
                        .await
                        .is_err()
                    {
                        return reason::PEER_GONE;
                    }
                }
                Frame::End => return reason::CLOSED,
                Frame::TooLarge => return reason::FRAME_TOO_LARGE,
                // A torn frame from the local component is that component
                // ending badly; there is nothing else it could mean on a
                // socket the Core owns both ends of.
                Frame::Broken(_) => return reason::CLOSED,
            }
        }
    };
    let down = async {
        let mut rate = Rate::new();
        loop {
            match read_frame(&mut net_read).await {
                Frame::Bytes(bytes) => {
                    touch();
                    if rate.tick() {
                        return reason::RATE_EXCEEDED;
                    }
                    if dataplane::write_frame(&mut local_write, &bytes)
                        .await
                        .is_err()
                    {
                        return reason::CLOSED;
                    }
                }
                Frame::End => return reason::PEER_GONE,
                Frame::TooLarge => return reason::FRAME_TOO_LARGE,
                Frame::Broken(e) => return classify(&e),
            }
        }
    };
    let vigil = async {
        loop {
            tokio::select! {
                _ = cut.wake.notified() => {
                    return cut.reason().unwrap_or(reason::CLOSED);
                }
                _ = tokio::time::sleep(TRUST_POLL) => {
                    let idle = born.elapsed().as_millis() as u64 - last.load(Ordering::Relaxed);
                    if idle >= IDLE_TIMEOUT.as_millis() as u64 {
                        return reason::IDLE_TIMEOUT;
                    }
                    if !still_attested(&state, &ends.device_id, &ends.node_id) {
                        return reason::DEVICE_REVOKED;
                    }
                    // The owner closed its control connection but kept the
                    // pipe: the grant was bound to that connection, so the pipe
                    // does not outlive it. (A component whose PROCESS died is
                    // caught a whole poll earlier, by its half of the pipe
                    // ending.)
                    if !owner_connected(&state, ends.conn_id) {
                        return reason::CLOSED;
                    }
                }
            }
        }
    };

    // The first of the three to finish names the reason; the other two are
    // abandoned mid-frame, which is safe precisely because this is a teardown:
    // no session survives to be desynchronized by a half-read frame.
    let reason = tokio::select! {
        r = up => r,
        r = down => r,
        r = vigil => r,
    };
    state
        .peer_channels
        .lock()
        .expect("lock peer_channels")
        .unregister(&ends.role, &ends.device_id, id);
    // Both halves shut, so the far end and the component each see the end
    // rather than waiting on a mute pipe.
    let _ = net_write.shutdown().await;
    let _ = local_write.shutdown().await;
    state.registry.lock().expect("lock registry").notify_conn(
        ends.conn_id,
        "peer.channel_closed",
        &json!({ "device_id": ends.device_id, "reason": reason }),
    );
}

/// Is the peer still an attested device of the account, under the very key this
/// channel was opened to? A re-enrolment (new `node_id` for the same label) is
/// a different device, and a record that stopped verifying is no record at all.
fn still_attested(state: &AppState, device_id: &str, node_id: &str) -> bool {
    dataplane::resolve_peer(state, device_id).is_some_and(|peer| peer.node_id == node_id)
}

/// Is the control connection that owns this channel still active?
fn owner_connected(state: &AppState, conn_id: ConnId) -> bool {
    state
        .registry
        .lock()
        .expect("lock registry")
        .conns
        .get(&conn_id)
        .is_some_and(|entry| matches!(entry.phase, crate::state::Phase::Active(_)))
}

/// A read failure on the peer stream, worded. One code travels this path: #88's
/// mid-stream watcher closes a connection whose direct path died under a
/// rendezvous-only relay policy, and the remedy for that is the pair's network,
/// not a retry.
fn classify(e: &std::io::Error) -> &'static str {
    if dataplane::failure_code(e) == dataplane::NO_DIRECT_PATH {
        dataplane::NO_DIRECT_PATH
    } else {
        reason::PEER_GONE
    }
}

/// A fixed-window frame counter. `tick` returns whether the cap has just been
/// exceeded.
struct Rate {
    window: Instant,
    count: u32,
}

impl Rate {
    fn new() -> Rate {
        Rate {
            window: Instant::now(),
            count: 0,
        }
    }

    fn tick(&mut self) -> bool {
        if self.window.elapsed() >= RATE_WINDOW {
            self.window = Instant::now();
            self.count = 0;
        }
        self.count += 1;
        self.count > RATE_MAX_PER_WINDOW
    }
}

/// What one read off either half can produce.
enum Frame {
    Bytes(Vec<u8>),
    /// A clean end between frames.
    End,
    /// The announced length is above the cap: refused BEFORE allocating, so
    /// neither a component nor a peer chooses our memory.
    TooLarge,
    Broken(std::io::Error),
}

/// Reads one frame: `u32` big-endian length, then the bytes. A length that
/// cannot be read at all is a clean end; a partial one is treated the same way
/// (the data channel's convention), because a sender that dies mid-length has
/// nothing more to say either.
async fn read_frame<R>(reader: &mut R) -> Frame
where
    R: AsyncRead + Unpin + ?Sized,
{
    use tokio::io::AsyncReadExt;
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Frame::End,
        Err(e) => return Frame::Broken(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Frame::TooLarge;
    }
    let mut buf = vec![0u8; len];
    match reader.read_exact(&mut buf).await {
        Ok(_) => Frame::Bytes(buf),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Frame::End,
        Err(e) => Frame::Broken(e),
    }
}
