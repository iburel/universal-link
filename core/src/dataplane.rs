// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Data plane: the P2P seam. The Core reaches the account's other devices
//! through a `PeerTransport` injected by the config — exactly as it opens its
//! server/IdP streams via a `Connector`, and for the same reason: iroh
//! (quinn/rustls) does not cross-compile from this crate (the wall that already
//! pushed TLS out of the library). The daemon binary wires in the iroh impl,
//! compiled natively; the library knows only the trait, and the tests use an
//! in-memory transport.
//!
//! The module is called `dataplane` and not `transport`: the latter is already
//! taken by the local IPC listener (UDS / named pipe), which is the CONTROL
//! PLANE. Here it is the data plane, between devices, end-to-end encrypted by
//! iroh.
//!
//! # The transfer protocol (T2)
//!
//! One bidirectional stream per transfer. FORWARD direction (initiator →
//! responder): an `offer` control frame (framed JSON, the file manifest) then
//! the BODIES of the files concatenated, in manifest order, with no delimiter —
//! the receiver reads exactly `size` bytes per file. RETURN direction
//! (responder → initiator): a single `done` frame once everything is on disk,
//! which serves as the acknowledgment.
//!
//! Framing (u32 length + payload, never the EOF) for CONTROL only, bounded by
//! `MAX_FRAME`. The bodies, by contrast, are streamed in fixed-size buffers
//! (`CHUNK`): we NEVER allocate on a length announced by the peer — no OOM
//! possible, no large file buffered in memory. The two directions of the
//! bidirectional stream avoid any collision between body and acknowledgment.
//!
//! QUIC lifecycle (learned the hard way in T1): dropping the connection right
//! after `finish()` ABANDONS the bytes in flight (implicit close(0) on the drop
//! of the last reference). Hence: the initiator only closes AFTER reading the
//! acknowledgment, and the responder holds the connection (bounded drain) until
//! the initiator closes — the close proves the acknowledgment arrived.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

use crate::connector::IoStream;
use crate::state::AppState;

/// Where to reach a peer on the data plane. The `node_id` is the device's
/// Ed25519 public key in hex (64 chars) — the same identity as on the server
/// side and as the iroh `EndpointId`. The `relay_url` is the one the peer
/// published in the directory (`presence.update`); the directory IS the
/// discovery (doc/server-api.md), we do not rely on iroh's DNS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerAddr {
    pub node_id: String,
    /// The relay the peer published in the directory. `None`: the peer has not
    /// (yet) published one — and opening then FAILS: without iroh discovery
    /// (`presets::Minimal`) or a direct address in the directory, the relay is
    /// the only route. (Direct connection over LAN will come with local
    /// discovery, not wired in yet — the fake reflects this contract.)
    pub relay_url: Option<String>,
}

/// A stream to a peer, later. `async fn` cannot be used in a trait object,
/// hence the boxed future — same shape as `Connecting`.
pub type Opening<'a> =
    Pin<Box<dyn Future<Output = std::io::Result<Box<dyn IoStream>>> + Send + 'a>>;

/// The next incoming stream, plus the `node_id` (authenticated by iroh) of the
/// peer that opened it.
pub type Incoming<'a> =
    Pin<Box<dyn Future<Output = std::io::Result<(String, Box<dyn IoStream>)>> + Send + 'a>>;

/// The local relay URL once known. A future because iroh discovers it in the
/// background after `bind`: resolves to `Some(url)` when a relay is
/// established, `None` if the transport has none (direct/LAN mode) or after a
/// delay with no relay — the caller publishes what it gets and never waits
/// indefinitely.
pub type HomeRelay<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

/// The transport's clean shutdown, once complete.
pub type Closing<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// The Core's data plane: open streams to peers, accept theirs, and know its
/// own relay to publish. `Debug` is mandatory (like `Connector`/`SecretStore`):
/// `Config` derives it, and an `Arc<dyn PeerTransport>` without it would not
/// compile.
///
/// The returned stream is bidirectional (`IoStream`): on the iroh side it is a
/// bidirectional QUIC stream (both halves joined); on the test side a plain
/// in-memory pipe. Whether connections are reused or not is an implementation
/// detail hidden behind `open` — the library reasons in logical streams.
pub trait PeerTransport: Send + Sync + std::fmt::Debug {
    /// Opens a stream to `peer` (establishing the connection if needed, then a
    /// bidirectional stream).
    fn open<'a>(&'a self, peer: &'a PeerAddr) -> Opening<'a>;

    /// Waits for the next incoming stream, whatever the peer. Called in a loop
    /// by the data plane task. An error = transport closed.
    fn accept(&self) -> Incoming<'_>;

    /// The local relay to publish in the directory via `presence.update`.
    fn home_relay(&self) -> HomeRelay<'_>;

    /// Closes the transport cleanly — at process shutdown, not at the drop of
    /// the Core. Default: nothing (the in-memory pipe has nothing to close);
    /// the iroh impl closes its endpoint, without which peers wait for a
    /// timeout and iroh logs an abandonment ("Endpoint dropped without close").
    fn close(&self) -> Closing<'_> {
        Box::pin(std::future::ready(()))
    }
}

/// ALPN of the data plane: a peer that does not speak it is refused by iroh
/// (at the handshake). Source of truth for the daemon's iroh impl. Versioned
/// like the rest — an incompatible change will bump it.
pub const ALPN: &[u8] = b"ul/data/1";

/// Maximum size of a framed CONTROL frame (offer, acknowledgment). Bounds the
/// memory a peer can make us allocate at once. The file bodies, by contrast,
/// are never framed: they are streamed in `CHUNK` buffers — a file of several
/// GiB goes through without ever exceeding this bound. The manifest is
/// therefore bounded as well (a transfer of tens of thousands of files would
/// exceed it; out of v1 scope, the offer would then fail).
const MAX_FRAME: u32 = 1024 * 1024;

/// Size of the body-streaming buffer. Neither memory nor throughput depends on
/// a value announced by the peer.
const CHUNK: usize = 64 * 1024;

/// Incoming streams served at the same time, at most. Beyond it, we stop
/// accepting until a handler finishes: a peer (of the account, though —
/// unknowns are refused before) that opens streams in bursts does not make the
/// tasks balloon without limit.
const MAX_PEER_TASKS: usize = 32;

/// What a responder allows itself, after its acknowledgment, to see the
/// initiator close. The close is confirmation (the initiator only closes after
/// reading the acknowledgment); the bound evicts a peer that would let it
/// linger.
const LINGER: Duration = Duration::from_secs(10);

/// A NO-PROGRESS budget on a read/write (network or disk). This is NOT a cap on
/// total duration: a legitimate file of several GiB takes as long as it needs,
/// as long as the bytes keep advancing. But a peer (of the account, though —
/// QUIC keeps the connection alive) that announces bytes and never sends them,
/// or never reads, is turned away after this delay — otherwise it would pin one
/// task per stream and freeze `MAX_PEER_TASKS`.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Budget for establishing an outgoing connection (resolution + iroh handshake,
/// which via a relay can take several seconds). Beyond it, the transfer fails
/// rather than freezing indefinitely.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Wait for the "done" acknowledgment on the initiator side, once all bodies
/// are written. This is NOT a stall: the receiver is legitimately SILENT while
/// it validates (renames each `.part`), so this is an ABSOLUTE budget covering
/// its commit phase. Generous — renames are fast metadata operations. Accepted
/// v1 limit: a transfer with a pathological file count ("folder-sync" scale,
/// deferred) could exceed it and wrongly fail a transfer that was in fact
/// delivered.
const ACK_TIMEOUT: Duration = Duration::from_secs(60);

/// Serializes the NAMING of received files (choosing the free name + renaming)
/// across all concurrent incoming transfers: `unique_dest` is a check-then-act,
/// and two simultaneous commits of the same basename would step on each other
/// without this lock (one would overwrite the other). Process-global — a Core
/// serves a single user, a single receive folder; renames are fast, so the
/// serialization has no perceptible cost.
static COMMIT_NAMING: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Maximum cadence of `transfer.progress` notifications (doc/core-api.md:
/// throttled by the Core, ~2/s per transfer). The first point (0%) and the last
/// (100%) always go out.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// The data plane accept loop, started at Core startup and alive as long as it
/// runs. Each accepted stream is served in a `JoinSet` task: bounded in number,
/// and ALL abandoned when the loop dies (the set is dropped when the `serve`
/// task is dropped, itself `abort()`ed at the drop of the Core) — a handler
/// (and the `.part` temporaries it holds) does not survive the Core that
/// spawned it.
pub(crate) async fn serve(state: Arc<AppState>) {
    let mut handlers = tokio::task::JoinSet::new();
    loop {
        // Ceiling reached: we reap before accepting more.
        while handlers.len() >= MAX_PEER_TASKS {
            let _ = handlers.join_next().await;
        }
        tokio::select! {
            incoming = state.transport.accept() => match incoming {
                Ok((peer, stream)) => {
                    // iroh authenticates the peer's KEY; the directory says
                    // whether that key is a device of the ACCOUNT (a valid
                    // attestation under OUR account key, C7). An unknown — even
                    // with the right ALPN — gets nothing: no byte read, no file
                    // written.
                    if !peer_in_directory(&state, &peer) {
                        tracing::warn!(peer = %peer, "incoming stream from a peer outside the directory: refused");
                        continue;
                    }
                    handlers.spawn(serve_incoming(state.clone(), peer, stream));
                }
                Err(e) => {
                    // Transport closed under our feet: the data plane is DEAD —
                    // at error level, not debug: a Core running without a data
                    // plane must be visible in the log.
                    tracing::error!(error = %e, "data plane acceptance terminated");
                    return;
                }
            },
            // Reap as we go, so the set does not keep handles to finished tasks
            // until the next incoming stream.
            Some(_) = handlers.join_next(), if !handlers.is_empty() => {}
        }
    }
}

/// Is the peer `node_id` a device of the account? C7: presence in the directory
/// NO LONGER SUFFICES — the server could inject a `node_id` there. A valid
/// attestation under OUR account key (AK_pub, derived from the recovery code,
/// never learned from the server) is required. Without a trust root (device not
/// joined yet) or without a snapshot (never connected), no one is recognized:
/// we do not serve what we cannot verify — fail-closed.
fn peer_in_directory(state: &AppState, node_id: &str) -> bool {
    // Leaf lock released before taking `session` (lock ordering).
    let ak_pub = {
        let root = state.account_root.lock().expect("lock account_root");
        let Some(root) = root.as_ref() else {
            return false;
        };
        root.ak_pub.clone()
    };
    let s = state.session.lock().expect("lock session");
    let Some(devices) = s.devices.as_ref() else {
        return false;
    };
    devices.values().any(|record| {
        record.get("node_id").and_then(Value::as_str) == Some(node_id)
            && record
                .get("attestation")
                .and_then(Value::as_str)
                .is_some_and(|att| crate::account_key::verify(&ak_pub, node_id, att))
    })
}

/// The `device_id` (server label) associated with a `node_id`, taken from the
/// last snapshot. Used to name the sender in `transfer.incoming`; `None` (not
/// found) falls back to the `node_id`.
fn device_id_for(state: &AppState, node_id: &str) -> Option<String> {
    let s = state.session.lock().expect("lock session");
    s.devices.as_ref()?.iter().find_map(|(id, record)| {
        (record.get("node_id").and_then(Value::as_str) == Some(node_id)).then(|| id.clone())
    })
}

/// The `PeerAddr` of a device of the account, taken from the last directory
/// snapshot. `None` if the device is unknown, incomplete, or — C7 — if its
/// `node_id` carries no valid attestation under our account key: the check
/// happens BEFORE any opening, so a server cannot redirect our files toward a
/// `node_id` it would have injected.
fn resolve_peer(state: &AppState, device_id: &str) -> Option<PeerAddr> {
    let ak_pub = {
        let root = state.account_root.lock().expect("lock account_root");
        root.as_ref()?.ak_pub.clone()
    };
    let s = state.session.lock().expect("lock session");
    let record = s.devices.as_ref()?.get(device_id)?;
    let node_id = record.get("node_id")?.as_str()?.to_string();
    let att = record.get("attestation").and_then(Value::as_str)?;
    if !crate::account_key::verify(&ak_pub, &node_id, att) {
        return None;
    }
    let relay_url = record
        .get("relay_url")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(PeerAddr { node_id, relay_url })
}

// ---------------------------------------------------------------------------
// The transfer protocol.
// ---------------------------------------------------------------------------

/// A file to send: its name (announced to the peer), its source on disk, and
/// its size at the moment of the offer.
pub struct OutgoingFile {
    pub name: String,
    pub source: PathBuf,
    pub size: u64,
}

/// A manifest entry, as the receiver reads it from the offer.
#[derive(Clone, Debug)]
pub struct FileHeader {
    pub name: String,
    pub size: u64,
}

/// INITIATOR side: sends the offer then the bodies, and waits for the
/// receiver's acknowledgment. `progress(done, total)` is called as the send
/// proceeds (the caller throttles). Public (with `read_offer`/`receive_bodies`)
/// because it is THE data plane protocol: the daemon's tests run it as-is over
/// two real iroh endpoints — the in-memory pipe cannot prove the QUIC
/// lifecycle.
pub async fn send_transfer(
    stream: &mut Box<dyn IoStream>,
    files: &[OutgoingFile],
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> std::io::Result<()> {
    let total = files.iter().fold(0u64, |a, f| a.saturating_add(f.size));
    let manifest: Vec<Value> = files
        .iter()
        .map(|f| json!({ "name": f.name, "size": f.size }))
        .collect();
    write_control(stream, &json!({ "type": "offer", "files": manifest })).await?;

    let mut done = 0u64;
    progress(done, total);
    let mut buf = vec![0u8; CHUNK];
    for f in files {
        let mut file =
            bounded(STALL_TIMEOUT, tokio::fs::File::open(&f.source), "open").await?;
        let mut remaining = f.size;
        while remaining > 0 {
            let want = remaining.min(CHUNK as u64) as usize;
            let n = bounded(STALL_TIMEOUT, file.read(&mut buf[..want]), "disk read").await?;
            if n == 0 {
                // The file shrank since the offer: impossible to hold the
                // announced size. We abandon — the stream resets, the receiver
                // sees the truncation and fails (never a silently truncated
                // file).
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("SIZE_CHANGED: {} shrank during send", f.name),
                ));
            }
            bounded(
                STALL_TIMEOUT,
                stream.write_all(&buf[..n]),
                "network write",
            )
            .await?;
            remaining -= n as u64;
            done += n as u64;
            progress(done, total);
        }
    }
    bounded(STALL_TIMEOUT, stream.flush(), "flush").await?;
    // Receiver's acknowledgment: everything is on its disk. We hold the stream
    // until then (otherwise, on QUIC, dropping would cut the bytes in flight).
    // ABSOLUTE budget (`ACK_TIMEOUT`): the receiver is legitimately silent
    // during its commit phase — this is not a stall.
    let ack = bounded(ACK_TIMEOUT, read_control(stream), "acknowledgment").await?;
    if ack.get("type").and_then(Value::as_str) != Some("done") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected acknowledgment",
        ));
    }
    // Our close tells the receiver the acknowledgment arrived (it drains until
    // then).
    let _ = stream.shutdown().await;
    Ok(())
}

/// RESPONDER side (step 1): reads the offer and returns the manifest. Separate
/// from receiving the bodies so the caller can announce `transfer.incoming`
/// between the two. Bounded (a peer that connects without ever offering is
/// turned away).
pub async fn read_offer(stream: &mut Box<dyn IoStream>) -> std::io::Result<Vec<FileHeader>> {
    let value = bounded(STALL_TIMEOUT, read_control(stream), "offer").await?;
    if value.get("type").and_then(Value::as_str) != Some("offer") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "first frame is not an offer",
        ));
    }
    let files = value
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "offer without files"))?;
    files
        .iter()
        .map(|f| {
            let name = f
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "file without a name")
                })?
                .to_string();
            let size = f.get("size").and_then(Value::as_u64).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "file without a size")
            })?;
            Ok(FileHeader { name, size })
        })
        .collect()
}

/// RESPONDER side (step 2): receives the bodies, validates them, and returns
/// the written paths. Chains the protocol's three stages: reception into
/// `.part` files, then rename, then acknowledgment. Used as-is by the daemon's
/// tests (a non-cancelable transfer); the Core's `serve` loop, by contrast,
/// calls the building blocks separately to place the cancellation boundary in
/// the right spot (see `serve_incoming`).
pub async fn receive_bodies(
    stream: &mut Box<dyn IoStream>,
    dest_dir: &Path,
    manifest: &[FileHeader],
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> std::io::Result<Vec<PathBuf>> {
    let parts = receive_to_parts(stream, dest_dir, manifest, progress).await?;
    let written = commit_parts(dest_dir, parts).await?;
    write_ack(stream).await?;
    Ok(written)
}

/// Receives the bodies into `.part` temporaries — the CANCELABLE phase. The
/// bodies announced by `manifest` are streamed into `dest_dir` (created if
/// needed). Nothing is visible yet: each file is a `.part` guarded by a
/// `PartFile` whose `Drop` erases it as long as it is not committed. If the
/// future is abandoned (cancellation) or returns an error, NO partial file
/// remains. Returns the guards to commit.
async fn receive_to_parts(
    stream: &mut Box<dyn IoStream>,
    dest_dir: &Path,
    manifest: &[FileHeader],
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> std::io::Result<Vec<(PartFile, String)>> {
    tokio::fs::create_dir_all(dest_dir).await?;
    let total = manifest.iter().fold(0u64, |a, f| a.saturating_add(f.size));
    let mut done = 0u64;
    progress(done, total);

    let mut parts: Vec<(PartFile, String)> = Vec::with_capacity(manifest.len());
    let mut buf = vec![0u8; CHUNK];
    for f in manifest {
        // Path-traversal defense, even in v1 "flat files": a name like
        // "../../etc/passwd" from a compromised sender must NOT write outside
        // the receive folder.
        let name = safe_file_name(&f.name).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("refused file name: {:?}", f.name),
            )
        })?;
        // A single descriptor OPEN at a time: the file is written THEN closed
        // (the handle is dropped); only the `.part` path survives until the
        // rename. A peer offering thousands of small files does not exhaust the
        // descriptors. (No fsync: v1 does not require durability against a power
        // outage for a transfer — the rename gives visibility atomicity, and a
        // per-file, non-incremental fsync would skew the "no-progress" budget on
        // a large file.)
        let part = PartFile::create(dest_dir)?;
        let mut file = part.open().await?;
        let mut remaining = f.size;
        while remaining > 0 {
            let want = remaining.min(CHUNK as u64) as usize;
            let n = bounded(STALL_TIMEOUT, stream.read(&mut buf[..want]), "body").await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "stream cut before the end of the file",
                ));
            }
            bounded(STALL_TIMEOUT, file.write_all(&buf[..n]), "disk write").await?;
            remaining -= n as u64;
            done = done.saturating_add(n as u64);
            progress(done, total);
        }
        drop(file);
        parts.push((part, name));
    }
    Ok(parts)
}

/// Renames the received temporaries to their final names — the COMMIT phase,
/// NON-cancelable (see `serve_incoming`). Choosing the free name and the rename
/// are serialized by `COMMIT_NAMING`: two incoming transfers of the same
/// basename cannot overwrite each other.
async fn commit_parts(
    dest_dir: &Path,
    parts: Vec<(PartFile, String)>,
) -> std::io::Result<Vec<PathBuf>> {
    let _naming = COMMIT_NAMING.lock().await;
    let mut written = Vec::with_capacity(parts.len());
    for (part, name) in parts {
        written.push(bounded(STALL_TIMEOUT, part.commit(dest_dir, &name), "rename").await?);
    }
    Ok(written)
}

/// Sends the "done" acknowledgment then holds the stream until the initiator
/// closes (it only closes after having READ the acknowledgment) — dropping
/// earlier would abandon the acknowledgment in flight on the QUIC side.
/// Best-effort beyond writing the acknowledgment.
async fn write_ack(stream: &mut Box<dyn IoStream>) -> std::io::Result<()> {
    write_control(stream, &json!({ "type": "done" })).await?;
    bounded(STALL_TIMEOUT, stream.flush(), "acknowledgment flush").await?;
    let _ = stream.shutdown().await;
    let _ = tokio::time::timeout(LINGER, drain(stream)).await;
    Ok(())
}

/// Serves an incoming stream: reads the offer, announces `transfer.incoming`,
/// receives the bodies, announces the result. `files.cancel` acts only during
/// RECEPTION: as soon as all the bytes are there, validation (rename +
/// acknowledgment) is a point of no return — a transfer whose bytes are all
/// durable is never reported "cancelled" nor left half-committed on disk. The
/// terminal outcome is emitted ONCE, by the task.
async fn serve_incoming(state: Arc<AppState>, peer: String, mut stream: Box<dyn IoStream>) {
    let manifest = match read_offer(&mut stream).await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(peer = %peer, error = %e, "unreadable incoming offer: abandoned");
            return;
        }
    };
    let device_id = device_id_for(&state, &peer).unwrap_or_else(|| peer.clone());
    let (transfer_id, cancel) = state.transfers.lock().expect("lock transfers").register();

    let files_json: Vec<Value> = manifest
        .iter()
        .map(|f| json!({ "name": f.name, "size": f.size }))
        .collect();
    notify_transfers(
        &state,
        "transfer.incoming",
        &json!({ "transfer_id": transfer_id, "device_id": device_id, "files": files_json }),
    );

    let dest_dir = state.receive_dir.clone();
    let sid = state.clone();
    let tid = transfer_id.clone();
    let mut throttle = Throttle::new();
    let mut progress = |d, t| throttle.tick(&sid, &tid, d, t);

    // `biased` + reception FIRST: on a tie (reception ready and cancellation
    // signaled in the same wakeup), reception wins — a complete transfer is
    // never reported cancelled. `None` = cancellation during reception: the
    // future is abandoned, the `.part` files are erased, nothing is committed.
    let received = tokio::select! {
        biased;
        r = receive_to_parts(&mut stream, &dest_dir, &manifest, &mut progress) => Some(r),
        _ = cancel.notified() => None,
    };
    let outcome = match received {
        // Complete reception: we COMMIT (rename + acknowledgment) outside the
        // `select!`, hence safe from cancellation — the bytes are all durable.
        Some(Ok(parts)) => match commit_parts(&dest_dir, parts).await {
            Ok(written) => {
                let _ = write_ack(&mut stream).await;
                Ok(written)
            }
            Err(e) => Err(e.to_string()),
        },
        Some(Err(e)) => Err(e.to_string()),
        None => Err("cancelled".to_string()),
    };

    // Unregister BEFORE notifying: a `files.cancel` that saw the terminal
    // outcome (via the topic) and retried immediately will indeed find
    // `TRANSFER_UNKNOWN`.
    state
        .transfers
        .lock()
        .expect("lock transfers")
        .entries
        .remove(&transfer_id);
    match outcome {
        Ok(paths) => {
            let paths_json: Vec<Value> = paths.iter().map(|p| json!(p.to_string_lossy())).collect();
            notify_transfers(
                &state,
                "transfer.finished",
                &json!({ "transfer_id": transfer_id, "paths": paths_json }),
            );
            tracing::debug!(peer = %peer, transfer = %transfer_id, files = manifest.len(), "incoming transfer received");
        }
        Err(error) => notify_transfers(
            &state,
            "transfer.failed",
            &json!({ "transfer_id": transfer_id, "error": error }),
        ),
    }
}

/// Why a `files.send` could not START (before the `transfer_id`). Once started,
/// failures (connection, disk) go through `transfer.failed`.
pub(crate) enum SendError {
    /// Target absent from the directory, or an invalid attestation under our
    /// key (C7) — fail-closed, indistinguishable so as to disclose nothing.
    UnknownDevice,
    /// Target known but with no published relay: unreachable for now.
    Offline,
    /// Invalid path (missing, directory, unreadable) — message for the caller.
    BadPath(String),
}

/// Starts an outgoing transfer: validates the paths, resolves the peer (C7),
/// registers the transfer and spawns the send task. Returns the `transfer_id`
/// right away — tracking goes through the `transfers` topic.
pub(crate) fn start_send(
    state: &Arc<AppState>,
    device_id: &str,
    paths: &[String],
) -> Result<String, SendError> {
    if paths.is_empty() {
        return Err(SendError::BadPath("no file".into()));
    }
    // Resolution first: no point reading the disk for a target we cannot reach
    // anyway (and no leak: unknown == unattested).
    let peer = resolve_peer(state, device_id).ok_or(SendError::UnknownDevice)?;
    if peer.relay_url.is_none() {
        return Err(SendError::Offline);
    }

    // v1 "flat files": each path must be a regular file. A directory is refused
    // outright (directory trees are a follow-up building block).
    let mut files = Vec::with_capacity(paths.len());
    for p in paths {
        let source = PathBuf::from(p);
        let meta =
            std::fs::metadata(&source).map_err(|e| SendError::BadPath(format!("{p} — {e}")))?;
        if meta.is_dir() {
            return Err(SendError::BadPath(format!(
                "folders are not supported (v1 files only): {p}"
            )));
        }
        if !meta.is_file() {
            return Err(SendError::BadPath(format!("non-regular path: {p}")));
        }
        let name = source
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| SendError::BadPath(format!("path without a file name: {p}")))?
            .to_string();
        files.push(OutgoingFile {
            name,
            source,
            size: meta.len(),
        });
    }

    let (transfer_id, cancel) = state.transfers.lock().expect("lock transfers").register();
    tokio::spawn(run_send(
        state.clone(),
        transfer_id.clone(),
        device_id.to_string(),
        peer,
        files,
        cancel,
    ));
    Ok(transfer_id)
}

/// The task of an outgoing transfer: announces `transfer.started`, opens the
/// stream, sends, announces the outcome. Cancelable like `serve_incoming`.
async fn run_send(
    state: Arc<AppState>,
    transfer_id: String,
    device_id: String,
    peer: PeerAddr,
    files: Vec<OutgoingFile>,
    cancel: Arc<Notify>,
) {
    let total = files.iter().fold(0u64, |a, f| a.saturating_add(f.size));
    let files_json: Vec<Value> = files
        .iter()
        .map(|f| json!({ "name": f.name, "size": f.size }))
        .collect();
    notify_transfers(
        &state,
        "transfer.started",
        &json!({ "transfer_id": transfer_id, "device_id": device_id, "files": files_json, "total": total }),
    );

    let sid = state.clone();
    let tid = transfer_id.clone();
    let mut throttle = Throttle::new();
    let mut progress = |d, t| throttle.tick(&sid, &tid, d, t);
    // `biased` + send FIRST: on a tie (send finished and cancellation signaled
    // in the same wakeup), the send wins — a transfer whose acknowledgment is
    // already read is never reported cancelled. `None` = cancellation in
    // flight: the future is abandoned.
    let outcome = tokio::select! {
        biased;
        r = async {
            let mut stream = bounded(CONNECT_TIMEOUT, state.transport.open(&peer), "peer connection").await?;
            send_transfer(&mut stream, &files, &mut progress).await
        } => r.map_err(|e| e.to_string()),
        _ = cancel.notified() => Err("cancelled".to_string()),
    };

    state
        .transfers
        .lock()
        .expect("lock transfers")
        .entries
        .remove(&transfer_id);
    match outcome {
        Ok(()) => notify_transfers(
            &state,
            "transfer.finished",
            &json!({ "transfer_id": transfer_id }),
        ),
        Err(error) => notify_transfers(
            &state,
            "transfer.failed",
            &json!({ "transfer_id": transfer_id, "error": error }),
        ),
    }
}

/// Cancels a transfer (outgoing or incoming): signals its task. `false` if the
/// `transfer_id` is unknown (already finished, or never existed). The task
/// cleans up and emits the terminal outcome itself.
pub(crate) fn cancel(state: &AppState, transfer_id: &str) -> bool {
    let cancel = {
        let t = state.transfers.lock().expect("lock transfers");
        t.entries.get(transfer_id).map(|e| e.cancel.clone())
    };
    match cancel {
        Some(cancel) => {
            cancel.notify_one();
            true
        }
        None => false,
    }
}

/// Pushes a notification on the `transfers` topic (scope `transfers.read`,
/// verified at subscription). The registry lock is taken and released here —
/// never held across a transfer await.
fn notify_transfers(state: &AppState, method: &str, params: &Value) {
    state
        .registry
        .lock()
        .expect("lock registry")
        .notify_topic("transfers", method, params);
}

/// Emits `transfer.progress`, but at most once per `PROGRESS_INTERVAL` — except
/// the very first point and the last (`done == total`), always emitted for a
/// clean display (0% at the start, 100% on arrival).
struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    fn new() -> Throttle {
        Throttle { last: None }
    }

    fn tick(&mut self, state: &AppState, transfer_id: &str, done: u64, total: u64) {
        let now = Instant::now();
        let due = self
            .last
            .is_none_or(|last| now.duration_since(last) >= PROGRESS_INTERVAL);
        if due || done == total {
            self.last = Some(now);
            notify_transfers(
                state,
                "transfer.progress",
                &json!({ "transfer_id": transfer_id, "done": done, "total": total }),
            );
        }
    }
}

/// The GUARD of a received file: the path of a `.part` temporary renamed
/// atomically at the end. It does NOT hold the descriptor (`open` reopens it
/// then closes it per file — a single one open at a time). Its `Drop` erases
/// the temporary as long as it has not been committed — this is what guarantees
/// that a cancellation (abandoned future) or an error never leaves an orphan
/// `.part`.
struct PartFile {
    path: PathBuf,
    committed: bool,
}

impl PartFile {
    /// Reserves a unique temporary (created empty) and arms the cleanup.
    /// SYNCHRONOUS on purpose: creating the file and arming the guard are
    /// indivisible — an `await` between the two would open a window where
    /// cancellation (abandoned future) would leave the file, created by the
    /// detached blocking task, WITHOUT a guard to erase it (an orphan `.part`).
    fn create(dir: &Path) -> std::io::Result<PartFile> {
        // A dotted (hidden), random name: never confused with a received file,
        // never colliding between two concurrent transfers. `create_new`
        // reserves the name and guarantees we overwrite nothing.
        let path = dir.join(format!(".ul-{}.part", crate::state::random_hex(8)));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(PartFile {
            path,
            committed: false,
        })
    }

    /// Opens the temporary for writing (the caller fsyncs and closes).
    async fn open(&self) -> std::io::Result<tokio::fs::File> {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .await
    }

    /// Renames the temporary (already durable) to a free name in `dir`.
    async fn commit(mut self, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
        let dest = unique_dest(dir, name);
        tokio::fs::rename(&self.path, &dest).await?;
        self.committed = true;
        Ok(dest)
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort, synchronous (Drop): the temporary is local and small.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Accepts only a plain BASENAME; refuses anything carrying a path structure —
/// the data plane's path-traversal defense. A legitimate sender only sends a
/// basename (`files.send` derives it from the source name), so this refusal
/// only affects a malicious or buggy peer. Deliberately WITHOUT
/// `Path::file_name`: its splitting is OS-dependent (`\` separates on Windows,
/// not on Linux), which would diverge across platforms; we reason on the raw
/// string, regardless of the OS. Refused: empty, `.`/`..`, any separator (`/`
/// OR `\`), colon (Windows drive/ADS), and control characters.
fn safe_file_name(raw: &str) -> Option<String> {
    if raw.is_empty() || raw == "." || raw == ".." {
        return None;
    }
    if raw
        .chars()
        .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
    {
        return None;
    }
    Some(raw.to_string())
}

/// A free destination path in `dir` for `name` (already sanitized): the name
/// as-is if it is free, otherwise suffixed " (n)" before the extension. Never
/// an overwrite — a received file does not destroy an existing file.
fn unique_dest(dir: &Path, name: &str) -> PathBuf {
    let direct = dir.join(name);
    if !direct.exists() {
        return direct;
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 1..=9999 {
        let candidate = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = dir.join(candidate);
        if !candidate.exists() {
            return candidate;
        }
    }
    // Implausible: we fall back on a random suffix rather than overwrite.
    dir.join(format!("{stem}-{}", crate::state::random_hex(4)))
}

// ---------------------------------------------------------------------------
// Framing and low-level utilities.
// ---------------------------------------------------------------------------

/// Writes a CONTROL frame: the serialized JSON, framed.
async fn write_control(stream: &mut Box<dyn IoStream>, value: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    write_frame(stream, &bytes).await
}

/// Reads a CONTROL frame and parses it as JSON.
async fn read_control(stream: &mut Box<dyn IoStream>) -> std::io::Result<Value> {
    let bytes = read_frame(stream).await?;
    serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Applies a "no-progress" bound to an I/O future: beyond `dur`, a `TimedOut`
/// error rather than an infinite wait.
async fn bounded<T>(
    dur: Duration,
    fut: impl Future<Output = std::io::Result<T>>,
    what: &str,
) -> std::io::Result<T> {
    match tokio::time::timeout(dur, fut).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("timed out with no progress: {what}"),
        )),
    }
}

/// Writes a framed message: u32 big-endian length, then the payload.
async fn write_frame<S>(stream: &mut S, payload: &[u8]) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin + ?Sized,
{
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "frame too large for the data plane: {}",
                    payload.len()
                ),
            )
        })?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

/// Reads a framed message. The announced length is bounded by `MAX_FRAME`
/// BEFORE any allocation: a peer does not choose our memory footprint.
async fn read_frame<S>(stream: &mut S) -> std::io::Result<Vec<u8>>
where
    S: AsyncRead + Unpin + ?Sized,
{
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len);
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("announced frame too large: {len}"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Consumes the stream until EOF or error (connection end included).
async fn drain<S>(stream: &mut S)
where
    S: AsyncRead + Unpin + ?Sized,
{
    let mut sink = [0u8; 64];
    loop {
        match stream.read(&mut sink).await {
            Ok(0) | Err(_) => return,
            // The protocol expects nothing after the acknowledgment: we absorb.
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{safe_file_name, unique_dest};

    #[test]
    fn safe_file_name_keeps_a_plain_basename() {
        assert_eq!(
            safe_file_name("report.pdf").as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            safe_file_name("my file (1).txt").as_deref(),
            Some("my file (1).txt")
        );
        assert_eq!(
            safe_file_name("archive.tar.gz").as_deref(),
            Some("archive.tar.gz")
        );
    }

    #[test]
    fn safe_file_name_refuses_anything_with_path_structure() {
        // IDENTICAL result on every OS (no platform-dependent
        // `Path::file_name`): nothing carrying a path structure is accepted, no
        // "keep the last segment" that would diverge Windows/Linux.
        assert_eq!(safe_file_name(""), None);
        assert_eq!(safe_file_name("."), None);
        assert_eq!(safe_file_name(".."), None);
        assert_eq!(safe_file_name("stuff/.."), None);
        assert_eq!(safe_file_name("../../etc/passwd"), None);
        assert_eq!(safe_file_name("/etc/passwd"), None);
        assert_eq!(safe_file_name("folder/file.txt"), None);
        // Windows separator (refused regardless of the OS), ADS/drive colon,
        // control character.
        assert_eq!(safe_file_name(r"..\..\evil"), None);
        assert_eq!(safe_file_name(r"folder\file.txt"), None);
        assert_eq!(safe_file_name("stream:ads"), None);
        assert_eq!(safe_file_name("line\nbreak"), None);
    }

    #[test]
    fn unique_dest_never_overwrites() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Free: the name as-is.
        assert_eq!(unique_dest(dir.path(), "a.txt"), dir.path().join("a.txt"));
        // Taken: suffix " (n)" before the extension, counting up.
        std::fs::write(dir.path().join("a.txt"), b"").unwrap();
        assert_eq!(
            unique_dest(dir.path(), "a.txt"),
            dir.path().join("a (1).txt")
        );
        std::fs::write(dir.path().join("a (1).txt"), b"").unwrap();
        assert_eq!(
            unique_dest(dir.path(), "a.txt"),
            dir.path().join("a (2).txt")
        );
        // No extension.
        std::fs::write(dir.path().join("noext"), b"").unwrap();
        assert_eq!(
            unique_dest(dir.path(), "noext"),
            dir.path().join("noext (1)")
        );
    }
}
