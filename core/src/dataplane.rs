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
//! A manifest entry whose `name` carries a relative, `/`-separated path is a
//! file inside a copied folder: the receiver recreates the parent directories
//! and joins the path onto its receive folder. An entry marked `dir: true` is an
//! EMPTY directory to recreate (size 0, no body) — a non-empty directory is
//! implied by its file entries and never listed. The path-traversal defense
//! (`is_safe_rel_path`, shared with the clipboard manifest) refuses anything a
//! naive join could turn into a write outside the receive folder (`..`, an
//! absolute or `\`-separated segment, `:`, a control character), so a
//! compromised sender cannot escape it.
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
use crate::rpc::RpcErr;
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

/// Maximum size of a framed CONTROL frame (offer, acknowledgment, clip
/// announce). Bounds the memory a peer can make us allocate at once. The file
/// bodies, by contrast, are never framed: they are streamed in `CHUNK` buffers —
/// a file of several GiB goes through without ever exceeding this bound. The
/// manifest is therefore bounded as well (a transfer of tens of thousands of
/// files would exceed it; out of v1 scope, the offer would then fail).
pub(crate) const MAX_FRAME: u32 = 1024 * 1024;

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
pub(crate) fn resolve_peer(state: &AppState, device_id: &str) -> Option<PeerAddr> {
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

/// Every reachable device of the account, EXCEPT this one: attested under our
/// key (C7) and with a published relay. The recipients of a clipboard
/// `clip_announce` broadcast (`clipnet::propagate`). Empty when we have no trust
/// root or no directory snapshot (not joined / never connected) — fail-closed.
pub(crate) fn account_peers(state: &AppState) -> Vec<PeerAddr> {
    let ak_pub = {
        let root = state.account_root.lock().expect("lock account_root");
        match root.as_ref() {
            Some(root) => root.ak_pub.clone(),
            None => return Vec::new(),
        }
    };
    let own = state.identity.node_id();
    let s = state.session.lock().expect("lock session");
    let Some(devices) = s.devices.as_ref() else {
        return Vec::new();
    };
    devices
        .values()
        .filter_map(|record| {
            let node_id = record.get("node_id").and_then(Value::as_str)?.to_string();
            if node_id == own {
                return None;
            }
            let att = record.get("attestation").and_then(Value::as_str)?;
            if !crate::account_key::verify(&ak_pub, &node_id, att) {
                return None;
            }
            let relay_url = record.get("relay_url").and_then(Value::as_str)?.to_string();
            Some(PeerAddr {
                node_id,
                relay_url: Some(relay_url),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The transfer protocol.
// ---------------------------------------------------------------------------

/// A manifest entry to send. `name` is what the peer sees: a plain basename for
/// a flat file, or a relative `/`-separated path for a file inside a copied
/// folder. `is_dir` marks an empty directory to recreate (no `source`, size 0);
/// every other entry is a file whose bytes are read from `source`. Invariant:
/// `source.is_none() == is_dir`.
pub struct OutgoingFile {
    pub name: String,
    pub source: Option<PathBuf>,
    pub size: u64,
    pub is_dir: bool,
}

/// A manifest entry, as the receiver reads it from the offer. `is_dir` is the
/// wire `dir: true` — an empty directory to recreate, no body.
#[derive(Clone, Debug)]
pub struct FileHeader {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
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
        .map(|f| offer_entry(&f.name, f.size, f.is_dir))
        .collect();
    write_control(stream, &json!({ "type": "offer", "files": manifest })).await?;

    let mut done = 0u64;
    progress(done, total);
    let mut buf = vec![0u8; CHUNK];
    for f in files {
        // A directory entry carries no body: the receiver recreates it from the
        // manifest alone.
        let Some(source) = &f.source else { continue };
        let mut file = bounded(STALL_TIMEOUT, tokio::fs::File::open(source), "open").await?;
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
            bounded(STALL_TIMEOUT, stream.write_all(&buf[..n]), "network write").await?;
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
    parse_manifest(&value)
}

/// Extracts the file manifest from an already-read `offer` control frame.
fn parse_manifest(value: &Value) -> std::io::Result<Vec<FileHeader>> {
    let files = value
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "offer without files")
        })?;
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
            // `dir` is optional and defaults to false (a flat-file offer, or an
            // older sender that never emits it). Anything but a bool or null is
            // a malformed frame — fail-closed rather than guess.
            let is_dir = match f.get("dir") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "file with a non-boolean dir",
                    ));
                }
            };
            // A directory carries no body: a non-zero size would desync the body
            // stream (we skip a dir's body, so the announced bytes would be read
            // as the NEXT file's). Refuse the whole offer rather than misalign.
            if is_dir && size != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "directory entry with a non-zero size",
                ));
            }
            Ok(FileHeader { name, size, is_dir })
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

/// A received manifest entry, pending commit: a file whose bytes are already in
/// a `.part` temporary, or an empty directory to recreate. `rel` is the
/// validated, `/`-separated relative path (a plain basename for a flat file).
enum Received {
    File { part: PartFile, rel: String },
    Dir { rel: String },
}

impl Received {
    fn rel(&self) -> &str {
        match self {
            Received::File { rel, .. } | Received::Dir { rel } => rel,
        }
    }
}

/// Receives the bodies into `.part` temporaries — the CANCELABLE phase. The
/// bodies announced by `manifest` are streamed into `dest_dir` (created if
/// needed). Nothing is visible yet: each file is a `.part` guarded by a
/// `PartFile` whose `Drop` erases it as long as it is not committed. Directory
/// entries carry no body — they are recorded for the commit to recreate. If the
/// future is abandoned (cancellation) or returns an error, NO partial file
/// remains. Returns the entries to commit.
async fn receive_to_parts(
    stream: &mut Box<dyn IoStream>,
    dest_dir: &Path,
    manifest: &[FileHeader],
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> std::io::Result<Vec<Received>> {
    tokio::fs::create_dir_all(dest_dir).await?;
    let total = manifest.iter().fold(0u64, |a, f| a.saturating_add(f.size));
    let mut done = 0u64;
    progress(done, total);

    let mut received: Vec<Received> = Vec::with_capacity(manifest.len());
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(manifest.len());
    let mut buf = vec![0u8; CHUNK];
    for f in manifest {
        // Path-traversal defense: a relative `/`-separated path only, so a name
        // like "../../etc/passwd" (or a rooted / `\`-separated / drive-qualified
        // one) from a compromised sender can NOT write outside the receive
        // folder. Accepts a plain basename (a flat file) as a single segment.
        if !crate::clipboard::is_safe_rel_path(&f.name) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("refused file name: {:?}", f.name),
            ));
        }
        // Reject a duplicate path — but ONLY for a directory or a NESTED file,
        // which the commit places by an exact rename with no collision suffix, so
        // a repeat would silently clobber the first (or, for a file-vs-directory
        // clash, fail the commit half-done). A plain top-level FILE is exempt: it
        // still goes through `unique_dest`, which disambiguates a repeat into
        // `f (1).txt` — the historical behavior an older sender relies on (it did
        // not uniquify basenames, so two sources named `f.txt` in one send are
        // legitimate). A conforming folder sender never emits a duplicate anyway
        // (`freeze_manifest`'s names are unique), so this only bites a hostile or
        // buggy offer, making the commit's "nothing is ever overwritten" hold
        // even then. Fail-closed, like the clipboard's `validate_remote_manifest`.
        let is_root_file = !f.is_dir && !f.name.contains('/');
        if !is_root_file && !seen.insert(f.name.clone()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("duplicate path in offer: {:?}", f.name),
            ));
        }
        let rel = f.name.clone();
        if f.is_dir {
            // No body (its size was validated to be 0 by `parse_manifest`): just
            // record it so the commit recreates the empty directory.
            received.push(Received::Dir { rel });
            continue;
        }
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
        received.push(Received::File { part, rel });
    }
    Ok(received)
}

/// Materializes the received entries under `dest_dir` — the COMMIT phase,
/// NON-cancelable (see `serve_incoming`). Choosing free names and renaming are
/// serialized by `COMMIT_NAMING`: two incoming transfers cannot overwrite each
/// other.
///
/// Collisions are resolved at the TOP LEVEL only — the tree structure below a
/// copied folder is preserved verbatim. A top-level DIRECTORY that already
/// exists is redirected to a fresh sibling (`folder (1)/`) and created empty, so
/// the whole received subtree lands there without merging into (or clobbering)
/// the existing one. A top-level FILE keeps the flat-file rule (`report (1).pdf`,
/// suffix before the extension). Distinct tops never collide (the sender
/// uniquifies top-level names) and every path is unique (`receive_to_parts`
/// rejects a duplicate before the commit, so even a hostile offer cannot make
/// one entry overwrite another). Nothing is ever overwritten.
async fn commit_parts(dest_dir: &Path, entries: Vec<Received>) -> std::io::Result<Vec<PathBuf>> {
    let _naming = COMMIT_NAMING.lock().await;

    // Pass 1: pick a free name for every distinct top-level DIRECTORY and
    // create it empty — reserving it on disk so a later top cannot pick the
    // same name, and so files can be renamed into it. A top is a directory when
    // an entry has a deeper segment under it, or is itself an empty-dir entry.
    let mut dir_top: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for entry in &entries {
        let (top, rest) = split_top(entry.rel());
        let is_dir_top = rest.is_some() || matches!(entry, Received::Dir { .. });
        if is_dir_top && !dir_top.contains_key(top) {
            let chosen = unique_child_name(dest_dir, top, true);
            bounded(
                STALL_TIMEOUT,
                tokio::fs::create_dir_all(dest_dir.join(&chosen)),
                "create directory",
            )
            .await?;
            dir_top.insert(top.to_string(), chosen);
        }
    }

    // Pass 2: create the empty directories and rename each file into place. All
    // directory tops now exist, so a plain top-level file's `unique_dest` sees
    // them (defensive — the sender never collides a file and a folder top).
    let mut written = Vec::with_capacity(entries.len());
    for entry in entries {
        let rel = entry.rel().to_string();
        let (top, rest) = split_top(&rel);
        match entry {
            Received::Dir { .. } => {
                // A bare empty-dir top is already created in pass 1; a deeper
                // empty dir (`folder/empty`) is created here under its remapped
                // top.
                let dest = dest_path(dest_dir, &dir_top, top, rest);
                bounded(
                    STALL_TIMEOUT,
                    tokio::fs::create_dir_all(&dest),
                    "create directory",
                )
                .await?;
                written.push(dest);
            }
            Received::File { part, .. } => {
                let dest = if rest.is_some() {
                    // A file inside a copied folder: its top is a reserved
                    // directory; recreate any intermediate parents, then rename.
                    let dest = dest_path(dest_dir, &dir_top, top, rest);
                    if let Some(parent) = dest.parent() {
                        bounded(
                            STALL_TIMEOUT,
                            tokio::fs::create_dir_all(parent),
                            "create directory",
                        )
                        .await?;
                    }
                    dest
                } else {
                    // A flat top-level file: the historical " (n)" collision rule.
                    unique_dest(dest_dir, top)
                };
                written.push(bounded(STALL_TIMEOUT, part.commit_to(&dest), "rename").await?);
            }
        }
    }
    Ok(written)
}

/// Splits a `/`-separated relative path into its top-level component and the
/// remainder (`None` when the path is a single segment).
fn split_top(rel: &str) -> (&str, Option<&str>) {
    match rel.split_once('/') {
        Some((top, rest)) => (top, Some(rest)),
        None => (rel, None),
    }
}

/// The destination path for an entry: the (possibly remapped) top-level
/// component joined with the remaining segments, pushed one at a time so `/`
/// never reaches the OS as a literal path component.
fn dest_path(
    dest_dir: &Path,
    dir_top: &std::collections::HashMap<String, String>,
    top: &str,
    rest: Option<&str>,
) -> PathBuf {
    let mapped = dir_top.get(top).map(String::as_str).unwrap_or(top);
    let mut path = dest_dir.to_path_buf();
    path.push(mapped);
    if let Some(rest) = rest {
        for seg in rest.split('/') {
            path.push(seg);
        }
    }
    path
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

/// Serves an incoming stream: reads the first control frame and dispatches on
/// its `type`. `offer` is a file transfer (below); `clip_announce` /
/// `clip_session` are the clipboard network plane (`clipnet`). The peer is
/// already vouched for by `peer_in_directory` (C7) at the accept loop.
async fn serve_incoming(state: Arc<AppState>, peer: String, mut stream: Box<dyn IoStream>) {
    let first = match bounded(STALL_TIMEOUT, read_control(&mut stream), "first frame").await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(peer = %peer, error = %e, "unreadable incoming frame: abandoned");
            return;
        }
    };
    match first.get("type").and_then(Value::as_str) {
        Some("offer") => serve_transfer(state, peer, first, stream).await,
        Some("clip_announce") => crate::clipnet::recv_announce(state, peer, first, stream).await,
        Some("clip_session") => crate::clipnet::serve_session(state, first, stream).await,
        other => {
            tracing::debug!(peer = %peer, kind = ?other, "unknown incoming frame type: abandoned");
        }
    }
}

/// A file transfer (`offer`): announces `transfer.incoming`, receives the
/// bodies, announces the result. `files.cancel` acts only during RECEPTION: as
/// soon as all the bytes are there, validation (rename + acknowledgment) is a
/// point of no return — a transfer whose bytes are all durable is never
/// reported "cancelled" nor left half-committed on disk. The terminal outcome
/// is emitted ONCE, by the task.
async fn serve_transfer(
    state: Arc<AppState>,
    peer: String,
    offer: Value,
    mut stream: Box<dyn IoStream>,
) {
    let manifest = match parse_manifest(&offer) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(peer = %peer, error = %e, "malformed incoming offer: abandoned");
            return;
        }
    };
    let device_id = device_id_for(&state, &peer).unwrap_or_else(|| peer.clone());
    let (transfer_id, cancel) = state.transfers.lock().expect("lock transfers").register();

    let files_json: Vec<Value> = manifest
        .iter()
        .map(|f| offer_entry(&f.name, f.size, f.is_dir))
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
    /// Invalid path (missing, unreadable) — message for the caller.
    BadPath(String),
    /// The manifest walk refused the paths (an unrepresentable name, a folder
    /// too deep, or over the manifest cap): the `freeze_manifest` error is
    /// relayed to the caller as-is.
    Rejected(RpcErr),
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

    // The SAME walk the clipboard uses to freeze a copied folder: a regular
    // file becomes one entry, a directory is walked into `<folder>/<rel>`
    // entries (an empty folder into a `dir:true` entry), top-level names are
    // uniquified, and every relative path is validated fail-closed so the
    // receiver's re-validation cannot drop it. The `file_id`s it mints are
    // unused here (the transfer pushes bodies in order, it does not pull by id).
    let files: Vec<OutgoingFile> = crate::clipboard::freeze_manifest(paths)
        .map_err(SendError::Rejected)?
        .into_iter()
        .map(|e| OutgoingFile {
            name: e.rel_path,
            source: e.backing.map(|b| b.source),
            size: e.size,
            is_dir: e.is_dir,
        })
        .collect();

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
        .map(|f| offer_entry(&f.name, f.size, f.is_dir))
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
pub(crate) fn notify_transfers(state: &AppState, method: &str, params: &Value) {
    state
        .registry
        .lock()
        .expect("lock registry")
        .notify_topic("transfers", method, params);
}

/// Emits `transfer.progress`, but at most once per `PROGRESS_INTERVAL` — except
/// the very first point and the last (`done == total`), always emitted for a
/// clean display (0% at the start, 100% on arrival).
pub(crate) struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    pub(crate) fn new() -> Throttle {
        Throttle { last: None }
    }

    pub(crate) fn tick(&mut self, state: &AppState, transfer_id: &str, done: u64, total: u64) {
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

    /// Renames the temporary (already durable) to `dest`, an exact path the
    /// caller has already chosen free (and whose parent directories exist).
    async fn commit_to(mut self, dest: &Path) -> std::io::Result<PathBuf> {
        tokio::fs::rename(&self.path, dest).await?;
        self.committed = true;
        Ok(dest.to_path_buf())
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

/// A manifest/notification entry as JSON: `{name, size}`, plus `dir: true` for
/// an empty directory (omitted for a file — the default, and what an older peer
/// expects). The single builder for the offer manifest and the `transfer.*`
/// notifications, so the three stay in lockstep.
fn offer_entry(name: &str, size: u64, is_dir: bool) -> Value {
    let mut v = json!({ "name": name, "size": size });
    if is_dir {
        v["dir"] = json!(true);
    }
    v
}

/// A free destination path in `dir` for a top-level FILE `name`: the flat-file
/// collision rule (the name as-is if free, otherwise suffixed " (n)" before the
/// extension). Never an overwrite — a received file does not destroy an existing
/// one.
fn unique_dest(dir: &Path, name: &str) -> PathBuf {
    dir.join(unique_child_name(dir, name, false))
}

/// A free child name in `dir` for `name`: `name` as-is if nothing there bears
/// it, otherwise a " (n)" suffix. A FILE suffixes before the extension
/// (`report (1).pdf`); a DIRECTORY suffixes the whole name (`folder (1)`, never
/// split on a dot). Never returns a name that already exists — a received tree
/// never merges into, or clobbers, an existing entry.
fn unique_child_name(dir: &Path, name: &str, is_dir: bool) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    let (stem, ext) = if is_dir {
        (name, None)
    } else {
        let path = Path::new(name);
        (
            path.file_stem().and_then(|s| s.to_str()).unwrap_or(name),
            path.extension().and_then(|s| s.to_str()),
        )
    };
    for n in 1..=9999 {
        let candidate = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    // Implausible: we fall back on a random suffix rather than overwrite.
    format!("{stem}-{}", crate::state::random_hex(4))
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
pub(crate) async fn write_frame<S>(stream: &mut S, payload: &[u8]) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin + ?Sized,
{
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("frame too large for the data plane: {}", payload.len()),
            )
        })?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

/// Reads a framed message. The announced length is bounded by `MAX_FRAME`
/// BEFORE any allocation: a peer does not choose our memory footprint.
pub(crate) async fn read_frame<S>(stream: &mut S) -> std::io::Result<Vec<u8>>
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
pub(crate) async fn drain<S>(stream: &mut S)
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
    use super::{
        dest_path, offer_entry, parse_manifest, split_top, unique_child_name, unique_dest,
    };
    use serde_json::json;

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

    #[test]
    fn unique_child_name_suffixes_a_folder_without_splitting_on_a_dot() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A folder named like a file must NOT be split on the dot: the whole
        // name is suffixed.
        std::fs::create_dir(dir.path().join("my.folder")).unwrap();
        assert_eq!(
            unique_child_name(dir.path(), "my.folder", true),
            "my.folder (1)"
        );
        // A free name is returned as-is.
        assert_eq!(unique_child_name(dir.path(), "clean", true), "clean");
    }

    #[test]
    fn split_top_separates_the_first_segment() {
        assert_eq!(split_top("report.pdf"), ("report.pdf", None));
        assert_eq!(split_top("folder/a.txt"), ("folder", Some("a.txt")));
        assert_eq!(split_top("folder/sub/a.txt"), ("folder", Some("sub/a.txt")));
    }

    #[test]
    fn dest_path_applies_the_top_remap_and_pushes_segments() {
        let root = std::path::Path::new("/recv");
        let mut remap = std::collections::HashMap::new();
        remap.insert("folder".to_string(), "folder (1)".to_string());
        // The top is remapped; deeper segments are pushed verbatim (never a
        // literal `/` reaching the OS as one component).
        assert_eq!(
            dest_path(root, &remap, "folder", Some("sub/a.txt")),
            root.join("folder (1)").join("sub").join("a.txt")
        );
        // An unmapped top stays as-is.
        assert_eq!(
            dest_path(root, &remap, "loose.txt", None),
            root.join("loose.txt")
        );
    }

    #[test]
    fn offer_entry_marks_only_directories() {
        assert_eq!(
            offer_entry("a.txt", 10, false),
            json!({"name":"a.txt","size":10})
        );
        assert_eq!(
            offer_entry("empty", 0, true),
            json!({"name":"empty","size":0,"dir":true})
        );
    }

    #[test]
    fn parse_manifest_reads_the_dir_flag_and_fails_closed() {
        // A file (no `dir`), an explicit empty directory, and default-false.
        let ok = parse_manifest(&json!({
            "type": "offer",
            "files": [
                { "name": "folder/a.txt", "size": 3 },
                { "name": "folder/empty", "size": 0, "dir": true },
            ]
        }))
        .expect("well-formed offer");
        assert_eq!(ok.len(), 2);
        assert!(!ok[0].is_dir);
        assert!(ok[1].is_dir);

        // A directory with a non-zero size would desync the body stream.
        parse_manifest(&json!({
            "type": "offer",
            "files": [ { "name": "d", "size": 5, "dir": true } ]
        }))
        .expect_err("a sized directory is refused");

        // A non-boolean `dir` is malformed.
        parse_manifest(&json!({
            "type": "offer",
            "files": [ { "name": "d", "size": 0, "dir": 1 } ]
        }))
        .expect_err("a non-boolean dir is refused");
    }
}
