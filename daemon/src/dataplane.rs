// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The daemon's data plane: the iroh implementation of the Core lib's
//! `PeerTransport`. iroh (QUIC/rustls via quinn) does not cross-compile from
//! the `core` crate — the same wall as TLS — so it lives here, compiled
//! natively by the three CI jobs. The Core knows only the trait.
//!
//! The endpoint is seeded with the device key (`device.key`): its iroh
//! `EndpointId` IS the `node_id` that the Core publishes in the directory.
//! Discovery happens through the directory (node_id + relay_url), not through
//! iroh's DNS — hence `presets::Minimal` (no discovery, just the crypto
//! provider). One local complement: with `lan_discovery` (on by default,
//! `config.json` turns it off), the endpoint also announces itself and
//! resolves peers over mDNS, so a peer on the same network is reachable by
//! its `node_id` alone — no relay, no internet. What mDNS resolves is an
//! ADDRESS, never trust: an impostor announcing someone else's `node_id`
//! fails the iroh handshake (the connection authenticates the key), and the
//! Core's directory check (C7) still gates every stream.
//!
//! The binary wires in `LazyIrohTransport`: the endpoint is only bound on the
//! first real use (session establishment calls `home_relay`). Three reasons. A
//! never-enrolled Core emits NO iroh traffic — a bound endpoint is not
//! passive: relay probes every ~20 s, portmapper (UPnP/PCP/NAT-PMP), a
//! persistent connection to the elected relay. The device key is only
//! read/created AFTER the Core's instance lock (taken by `spawn`) — two
//! daemons started together do not fight over `device.key`. And a bind failure
//! does not stop the daemon from starting: it is logged and RETRIED on the
//! next use, the IPC (hence the GUI) stays alive — the same policy as broken
//! config.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Context as _;
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, RelayUrl, SecretKey};
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use n0_future::{Stream, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use universallink_core::{
    ALPN, Closing, HomeRelay, Incoming, IoStream, Listening, Opening, PeerAddr, PeerTransport,
};

/// Maximum wait for the endpoint to become reachable via a relay, after which
/// `home_relay` returns `None` (offline, no relay to publish).
const HOME_RELAY_WAIT: Duration = Duration::from_secs(10);

/// Handshake budget for an INCOMING connection (QUIC accept + first stream).
/// Each incoming one is served in its own task: a peer that connects without
/// ever opening a stream has blocked no one, and at the end of this budget it
/// is turned away.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Incoming handshakes carried out at once, at most. Beyond that, the acceptor
/// stops taking connections until it has untangled some — it bounds the tasks
/// and memory that a burst of connections can cost.
const MAX_HANDSHAKES: usize = 16;

/// Depth of the queue of ready streams (handshake done, not yet consumed by
/// the Core's `serve` loop). Small: it is a transfer buffer, not a waiting
/// room.
const READY_QUEUE: usize = 8;

/// What we allow ourselves to close the endpoint cleanly (peers are notified,
/// no timeout on their side). Beyond that, we leave anyway.
const CLOSE_BUDGET: Duration = Duration::from_secs(3);

pub struct IrohTransport {
    endpoint: Endpoint,
    /// Incoming streams whose handshake is done, served by `accept`.
    ready: tokio::sync::Mutex<mpsc::Receiver<(String, Box<dyn IoStream>)>>,
    /// The accept task; dies on its own when the endpoint closes, aborted if
    /// the transport is dropped without `close`.
    acceptor: tokio::task::JoinHandle<()>,
    /// The `node_id`s currently visible over mDNS (hex), maintained by
    /// `lan_task`. Stays empty forever when LAN discovery is off.
    lan: Arc<Mutex<HashSet<String>>>,
    /// Bumped by `lan_task` at every actual set change; `lan_changes`
    /// subscribes to it. Owned here in the plain case; `bind` also accepts the
    /// caller's (`LazyIrohTransport` hands over its own, created before the
    /// endpoint exists, so a receiver taken while lazy still fires once bound).
    lan_gen: tokio::sync::watch::Sender<u64>,
    /// Consumes the mDNS discovery events into `lan`. Ends on its own when
    /// the endpoint closes (the discovery actor dies with its last handle and
    /// the event stream with it); the abort at drop is a safety net.
    lan_task: Option<tokio::task::JoinHandle<()>>,
    /// Says one plain line if LAN discovery is on but multicast never reaches
    /// the wire. Short-lived; aborted at drop so a torn-down transport cannot
    /// speak for its successor.
    lan_probe: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for IrohTransport {
    fn drop(&mut self) {
        self.acceptor.abort();
        for task in [&self.lan_task, &self.lan_probe].into_iter().flatten() {
            task.abort();
        }
    }
}

/// The mDNS service name endpoints announce themselves under
/// (`<node_id>._universallink._udp.local`). Ours rather than the crate's
/// default `irohv1`: only UniversalLink devices answer each other, and a
/// packet capture names the protocol honestly.
const MDNS_SERVICE: &str = "universallink";

impl IrohTransport {
    /// Production endpoint. `relay`: the deployment's relay (self-hosted) if it
    /// is configured, otherwise the n0 public relays — a server of one's own
    /// must not structurally depend on third-party infra. Certificates
    /// verified normally, no DNS discovery. `lan_discovery` adds the mDNS
    /// lookup (see the module header) — resolution AND announcement: one flag,
    /// both directions, because announcing without resolving (or the reverse)
    /// would just be a device its siblings half-see.
    pub async fn bind(
        seed: [u8; 32],
        relay: Option<RelayUrl>,
        lan_discovery: bool,
    ) -> anyhow::Result<IrohTransport> {
        let (lan_gen, _) = tokio::sync::watch::channel(0);
        Self::bind_with_gen(seed, relay, lan_discovery, lan_gen).await
    }

    /// `bind`, with the LAN generation channel supplied by the caller —
    /// `LazyIrohTransport` needs `lan_changes` to answer BEFORE the endpoint
    /// exists, so it owns the sender and hands it over at the (lazy) bind.
    pub async fn bind_with_gen(
        seed: [u8; 32],
        relay: Option<RelayUrl>,
        lan_discovery: bool,
        lan_gen: tokio::sync::watch::Sender<u64>,
    ) -> anyhow::Result<IrohTransport> {
        let secret = SecretKey::from_bytes(&seed);
        let mdns = lan_lookup(&secret, lan_discovery);
        let relay_mode = match relay {
            Some(url) => RelayMode::custom([url]),
            None => RelayMode::Default,
        };
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(relay_mode);
        if let Some(mdns) = &mdns {
            builder = builder.address_lookup(mdns.clone());
        }
        Self::finish(builder, mdns, lan_gen).await
    }

    /// Test endpoint: a LOCAL relay (self-signed certificate) whose
    /// verification we skip, and the portmapper turned off (no UPnP/PCP/NAT-PMP
    /// probes to the test machine's gateway — the tests declare themselves
    /// offline, and they are). `lan_discovery` as in `bind` — with an EMPTY
    /// relay map it is the only route between two test endpoints, which is
    /// exactly what the LAN test proves. Gated by the `test-utils` feature
    /// (enabled by the dev-dependencies only): the unverified TLS path DOES
    /// NOT EXIST in the production binary — the compiler guarantees it, not a
    /// convention.
    #[cfg(feature = "test-utils")]
    pub async fn bind_test(
        seed: [u8; 32],
        relay_map: iroh::RelayMap,
        lan_discovery: bool,
    ) -> anyhow::Result<IrohTransport> {
        let secret = SecretKey::from_bytes(&seed);
        let mdns = lan_lookup(&secret, lan_discovery);
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(RelayMode::Custom(relay_map))
            .portmapper_config(iroh::endpoint::PortmapperConfig::Disabled)
            .ca_tls_config(iroh::tls::CaTlsConfig::insecure_skip_verify());
        if let Some(mdns) = &mdns {
            builder = builder.address_lookup(mdns.clone());
        }
        let (lan_gen, _) = tokio::sync::watch::channel(0);
        Self::finish(builder, mdns, lan_gen).await
    }

    async fn finish(
        builder: iroh::endpoint::Builder,
        mdns: Option<MdnsAddressLookup>,
        lan_gen: tokio::sync::watch::Sender<u64>,
    ) -> anyhow::Result<IrohTransport> {
        // Subscribed BEFORE the endpoint binds: the discovery actor is already
        // running (it starts with the lookup), so waiting until after `bind`
        // would let a first announcement slip through unheard — and the actor
        // deduplicates republishes, so a missed first hello is missed for
        // good. (A residual window remains inside the crate itself: a peer
        // whose very first announcement lands while a resolution for it is in
        // flight is recorded but not surfaced to subscribers. Its consequence
        // is only ever conservative — the peer is not counted as LAN-visible,
        // and the relay route still stands.)
        let lan_events = match &mdns {
            Some(mdns) => Some(mdns.subscribe().await),
            None => None,
        };
        let endpoint = builder.bind().await.context("binding the iroh endpoint")?;
        let (tx, rx) = mpsc::channel(READY_QUEUE);
        let acceptor = tokio::spawn(acceptor(endpoint.clone(), tx));
        let lan = Arc::new(Mutex::new(HashSet::new()));
        let lan_task =
            lan_events.map(|events| tokio::spawn(watch_lan(events, lan.clone(), lan_gen.clone())));
        let lan_probe = mdns.is_some().then(|| tokio::spawn(warn_if_lan_dark()));
        Ok(IrohTransport {
            endpoint,
            ready: tokio::sync::Mutex::new(rx),
            acceptor,
            lan,
            lan_gen,
            lan_task,
            lan_probe,
        })
    }

    /// The underlying endpoint (local address, `online()`, for tests).
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

/// The mDNS lookup when `lan_discovery` is on, built ahead of the endpoint
/// (it only needs the public key) so a handle survives for `subscribe`. A
/// network where multicast cannot start at all (neither IPv4 nor IPv6) costs
/// the LAN route, never the data plane: warned, not fatal — the relay still
/// works, and `LazyIrohTransport` would otherwise retry a doomed bind forever.
fn lan_lookup(secret: &SecretKey, lan_discovery: bool) -> Option<MdnsAddressLookup> {
    if !lan_discovery {
        return None;
    }
    match MdnsAddressLookup::builder()
        .service_name(MDNS_SERVICE)
        .build(secret.public())
    {
        Ok(mdns) => Some(mdns),
        Err(e) => {
            tracing::warn!(error = %e, "LAN discovery unavailable: continuing without it");
            None
        }
    }
}

/// Whether multicast actually reaches the wire: a beacon sent to the mDNS
/// group must come back to a member of that group on the same host. The probe
/// uses the real group but an ephemeral port, so it never collides with mDNS
/// itself. Public because it is also the tests' judge of whether an
/// environment can host the real-socket LAN tests at all — GitHub's hosted
/// macOS runners refuse the send outright (five identical timeouts in a row
/// on every try, while real Macs pass).
pub async fn multicast_reaches_the_wire() -> bool {
    use std::net::Ipv4Addr;

    let group = Ipv4Addr::new(224, 0, 0, 251);
    let Ok(rx) = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else {
        return false;
    };
    let Ok(local) = rx.local_addr() else {
        return false;
    };
    if rx.join_multicast_v4(group, Ipv4Addr::UNSPECIFIED).is_err() {
        return false;
    }
    let Ok(tx) = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else {
        return false;
    };
    let beacon = b"universallink multicast probe";
    // Two beacons: the first can race the group join on a slow stack.
    for _ in 0..2 {
        let _ = tx.send_to(beacon, (group, local.port())).await;
        let mut buf = [0u8; 64];
        match tokio::time::timeout(Duration::from_secs(2), rx.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) if &buf[..n] == beacon => return true,
            _ => {}
        }
    }
    false
}

/// One plain line when LAN discovery is on but the wire is dark. Without it,
/// the only trace is the discovery library's per-send warnings — hundreds of
/// lines that name no cure. macOS is the expected culprit: it asks each fresh
/// build for the "Local Network" permission and quietly refuses every
/// multicast send (`No route to host`) until someone answers, so its line
/// names the switch to flip. Three probes over twenty seconds ride out a
/// Wi-Fi still associating at login and a permission prompt just answered.
async fn warn_if_lan_dark() {
    if any_attempt_succeeds(3, Duration::from_secs(10), multicast_reaches_the_wire).await {
        return;
    }
    tracing::warn!("{}", lan_dark_notice(std::env::consts::OS));
}

/// Runs `probe` up to `attempts` times, `pause` apart, stopping at the first
/// success.
async fn any_attempt_succeeds<F>(attempts: u32, pause: Duration, probe: impl Fn() -> F) -> bool
where
    F: std::future::Future<Output = bool>,
{
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(pause).await;
        }
        if probe().await {
            return true;
        }
    }
    false
}

/// The line for a dark wire, per platform. Takes the OS as a value so every
/// platform's message is checkable from any platform.
fn lan_dark_notice(os: &str) -> &'static str {
    match os {
        "macos" => {
            "LAN discovery is on, but multicast does not reach the wire — macOS is \
             likely denying local network access: allow UniversalLink under System \
             Settings → Privacy & Security → Local Network. Until then this device \
             neither sees nor is seen on its own network; server and relay are \
             unaffected."
        }
        _ => {
            "LAN discovery is on, but multicast does not reach the wire: this device \
             neither sees nor is seen on its own network. Server and relay are \
             unaffected."
        }
    }
}

/// Maintains the set of LAN-visible `node_id`s from the discovery events.
/// Exactly two event kinds: a peer heard (or updated) enters the set, an
/// expired one (silent beyond its TTL) leaves it. Ends with the stream.
async fn watch_lan(
    mut events: impl Stream<Item = DiscoveryEvent> + Unpin,
    lan: Arc<Mutex<HashSet<String>>>,
    lan_gen: tokio::sync::watch::Sender<u64>,
) {
    while let Some(event) = events.next().await {
        // Mutate, release, THEN bump: a watcher woken by the bump re-pulls
        // the set, so it must never observe the pre-mutation state.
        let changed = {
            let mut lan = lan.lock().expect("lock lan set");
            match event {
                DiscoveryEvent::Discovered { endpoint_info, .. } => {
                    let node_id = hex::encode(endpoint_info.endpoint_id.as_bytes());
                    let inserted = lan.insert(node_id.clone());
                    if inserted {
                        tracing::debug!(peer = %node_id, "peer visible on the LAN");
                    }
                    inserted
                }
                DiscoveryEvent::Expired { endpoint_id } => {
                    let node_id = hex::encode(endpoint_id.as_bytes());
                    let removed = lan.remove(&node_id);
                    if removed {
                        tracing::debug!(peer = %node_id, "peer gone from the LAN");
                    }
                    removed
                }
                // `#[non_exhaustive]`: an event kind a future crate version
                // adds is no reason to stop listening — ignored, not fatal.
                _ => false,
            }
        };
        if changed {
            lan_gen.send_modify(|generation| *generation += 1);
        }
    }
}

/// The accept loop: takes each incoming connection and carries out its
/// handshake (QUIC accept + `accept_bi`) in a SEPARATE task, bounded in time
/// and in number. Waiting for a stream from a slow peer therefore never holds
/// up the acceptance of the next ones — otherwise a single peer connected
/// without a stream would head-of-line block the entire data plane.
async fn acceptor(endpoint: Endpoint, ready: mpsc::Sender<(String, Box<dyn IoStream>)>) {
    let mut handshakes = tokio::task::JoinSet::new();
    loop {
        while handshakes.len() >= MAX_HANDSHAKES {
            let _ = handshakes.join_next().await;
        }
        tokio::select! {
            incoming = endpoint.accept() => {
                // `None`: endpoint closed. The loop dies, the senders too
                // (JoinSet dropped), and `accept` on the trait side will see
                // the queue close.
                let Some(incoming) = incoming else { return };
                let ready = ready.clone();
                handshakes.spawn(async move {
                    let conn = match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await {
                        Ok(Ok(conn)) => conn,
                        // Failed handshake (a peer that gives up, incompatible
                        // ALPN) or too slow: next.
                        Ok(Err(e)) => {
                            tracing::debug!(error = %e, "incoming iroh handshake failed");
                            return;
                        }
                        Err(_) => return,
                    };
                    let peer = hex::encode(conn.remote_id().as_bytes());
                    let (send, recv) =
                        match tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi()).await {
                            Ok(Ok(pair)) => pair,
                            Ok(Err(e)) => {
                                tracing::debug!(error = %e, "iroh accept_bi failed");
                                return;
                            }
                            Err(_) => {
                                tracing::debug!(peer = %peer, "peer connected without opening a stream: turned away");
                                return;
                            }
                        };
                    // Queue full and the Core's `serve` loop gone: too bad for
                    // this stream, the peer will see the connection close.
                    let _ = ready.send((peer, bidi(conn, send, recv))).await;
                });
            }
            Some(_) = handshakes.join_next(), if !handshakes.is_empty() => {}
        }
    }
}

impl std::fmt::Debug for IrohTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IrohTransport({})", self.endpoint.id().fmt_short())
    }
}

fn wrap<E: std::fmt::Display>(ctx: &str, e: E) -> io::Error {
    io::Error::other(format!("{ctx}: {e}"))
}

/// A peer's iroh address, built from its directory entry.
fn peer_to_addr(peer: &PeerAddr) -> io::Result<EndpointAddr> {
    let bytes: [u8; 32] = hex::decode(&peer.node_id)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "node_id hex of 32 bytes"))?;
    let id = PublicKey::from_bytes(&bytes).map_err(|e| wrap("invalid node_id", e))?;
    let mut addr = EndpointAddr::new(id);
    if let Some(relay) = &peer.relay_url {
        let relay: RelayUrl = relay.parse().map_err(|e| wrap("invalid relay_url", e))?;
        addr = addr.with_relay_url(relay);
    }
    Ok(addr)
}

impl PeerTransport for IrohTransport {
    fn open<'a>(&'a self, peer: &'a PeerAddr) -> Opening<'a> {
        Box::pin(async move {
            let addr = peer_to_addr(peer)?;
            let conn = self
                .endpoint
                .connect(addr, ALPN)
                .await
                .map_err(|e| wrap("iroh connection", e))?;
            let (send, recv) = conn.open_bi().await.map_err(|e| wrap("open_bi", e))?;
            Ok(bidi(conn, send, recv))
        })
    }

    fn accept(&self) -> Incoming<'_> {
        Box::pin(async move {
            let mut ready = self.ready.lock().await;
            ready
                .recv()
                .await
                .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "iroh endpoint closed"))
        })
    }

    fn home_relay(&self) -> HomeRelay<'_> {
        Box::pin(async move {
            // `online()` resolves when the endpoint is reachable via a relay;
            // bounded, because offline it would never resolve.
            if tokio::time::timeout(HOME_RELAY_WAIT, self.endpoint.online())
                .await
                .is_err()
            {
                return None;
            }
            self.endpoint
                .addr()
                .relay_urls()
                .next()
                .map(ToString::to_string)
        })
    }

    fn close(&self) -> Closing<'_> {
        Box::pin(async move {
            // Closing notifies the peers (otherwise they wait for a timeout)
            // and iroh stops logging an abandonment. The acceptor sees
            // `accept()` return `None` and shuts itself down.
            if tokio::time::timeout(CLOSE_BUDGET, self.endpoint.close())
                .await
                .is_err()
            {
                tracing::warn!("closing the iroh endpoint is taking too long: abandoned");
            }
        })
    }

    fn lan_peers(&self) -> Vec<String> {
        self.lan
            .lock()
            .expect("lock lan set")
            .iter()
            .cloned()
            .collect()
    }

    fn lan_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.lan_gen.subscribe()
    }
}

/// A bidirectional iroh stream presented as an `IoStream`. The `Connection` is
/// kept alive here: dropping it would close the stream out from under us.
fn bidi(conn: Connection, send: SendStream, recv: RecvStream) -> Box<dyn IoStream> {
    Box::new(BiStream {
        _conn: conn,
        io: tokio::io::join(recv, send),
    })
}

struct BiStream {
    _conn: Connection,
    io: tokio::io::Join<RecvStream, SendStream>,
}

impl AsyncRead for BiStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for BiStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// Lazy binding — what the binary actually wires in.
// ---------------------------------------------------------------------------

/// Lazily-bound `IrohTransport` (see the module header). `open` and
/// `home_relay` bind the endpoint on the first call — and the first call comes
/// from session establishment, hence from an ENROLLED Core; `accept` waits
/// patiently for the binding to have happened (a never-enrolled Core therefore
/// never listens). A bind failure is returned to the caller and RETRIED on the
/// next call — the daemon lives just fine with a broken data plane.
pub struct LazyIrohTransport {
    config_dir: PathBuf,
    relay: Option<RelayUrl>,
    /// Read once, at the bind — like `relay`, a change requires a Core
    /// restart.
    lan_discovery: bool,
    /// The LAN generation channel, created HERE so `lan_changes` can hand out
    /// receivers before the endpoint exists: the sender is given to the inner
    /// transport at the (lazy) bind, and a receiver taken while still unbound
    /// simply waits — the sender never drops.
    lan_gen: tokio::sync::watch::Sender<u64>,
    cell: tokio::sync::OnceCell<IrohTransport>,
    /// Wakes the waiting `accept`s once the endpoint is bound.
    bound: tokio::sync::Notify,
}

impl LazyIrohTransport {
    pub fn new(
        config_dir: PathBuf,
        relay: Option<RelayUrl>,
        lan_discovery: bool,
    ) -> LazyIrohTransport {
        LazyIrohTransport {
            config_dir,
            relay,
            lan_discovery,
            lan_gen: tokio::sync::watch::channel(0).0,
            cell: tokio::sync::OnceCell::new(),
            bound: tokio::sync::Notify::new(),
        }
    }

    /// The endpoint, bound on the first call. A failure does not poison the
    /// cell: the next call retries.
    async fn ensure(&self) -> io::Result<&IrohTransport> {
        let transport = self
            .cell
            .get_or_try_init(|| async {
                // The device key is read HERE, on the first use — never before
                // the Core's instance lock.
                let seed = universallink_core::load_or_generate_device_seed(&self.config_dir)
                    .map_err(|e| wrap("device identity", format!("{e:#}")))?;
                let transport = IrohTransport::bind_with_gen(
                    seed,
                    self.relay.clone(),
                    self.lan_discovery,
                    self.lan_gen.clone(),
                )
                .await
                .map_err(|e| wrap("binding the iroh endpoint", format!("{e:#}")))?;
                tracing::info!(
                    node_id = %transport.endpoint.id().fmt_short(),
                    "iroh data plane bound"
                );
                Ok::<_, io::Error>(transport)
            })
            .await
            .inspect_err(|e| tracing::error!(error = %e, "data plane unavailable"))?;
        // Idempotent — each success wakes the waiting `accept`s (notifying
        // INSIDE the init would miss those that arrived between the init and
        // the value being set).
        self.bound.notify_waiters();
        Ok(transport)
    }

    /// Waits for the endpoint to be bound (by `open`/`home_relay`), without
    /// ever triggering the binding itself.
    async fn wait_bound(&self) -> &IrohTransport {
        loop {
            // Arm BEFORE checking: a binding that succeeds between the check
            // and the wait would otherwise be a lost wakeup.
            let notified = self.bound.notified();
            if let Some(transport) = self.cell.get() {
                return transport;
            }
            notified.await;
        }
    }
}

impl std::fmt::Debug for LazyIrohTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.cell.get() {
            Some(t) => t.fmt(f),
            None => write!(f, "LazyIrohTransport(not bound)"),
        }
    }
}

impl PeerTransport for LazyIrohTransport {
    fn open<'a>(&'a self, peer: &'a PeerAddr) -> Opening<'a> {
        Box::pin(async move { self.ensure().await?.open(peer).await })
    }

    fn accept(&self) -> Incoming<'_> {
        Box::pin(async move { self.wait_bound().await.accept().await })
    }

    fn home_relay(&self) -> HomeRelay<'_> {
        Box::pin(async move {
            match self.ensure().await {
                Ok(transport) => transport.home_relay().await,
                // No relay to publish; the failure is already logged, and the
                // session will retry on its next probe.
                Err(_) => None,
            }
        })
    }

    fn listen(&self) -> Listening<'_> {
        // The bind, and nothing else: a Core that has to be REACHABLE by a device
        // it does not know yet (a pairing window) needs the endpoint up, and does
        // not need a relay to have been elected — waiting for one would cost ten
        // seconds precisely where there is no internet, which is the case this is
        // for. A failure is already logged by `ensure`; the caller has nothing to
        // do about it either way.
        Box::pin(async move {
            let _ = self.ensure().await;
        })
    }

    fn close(&self) -> Closing<'_> {
        Box::pin(async move {
            if let Some(transport) = self.cell.get() {
                transport.close().await;
            }
        })
    }

    fn lan_peers(&self) -> Vec<String> {
        // Not bound = radio not on yet: nobody visible. And it must stay that
        // way — this is called on hot paths and must never trigger the bind.
        self.cell
            .get()
            .map(PeerTransport::lan_peers)
            .unwrap_or_default()
    }

    fn lan_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        // Our own channel — the same sender the inner transport bumps once
        // bound, so a receiver taken now fires later without rewiring.
        self.lan_gen.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn the_macos_notice_names_the_switch_to_flip() {
        assert!(lan_dark_notice("macos").contains("Local Network"));
    }

    #[test]
    fn other_platforms_get_the_plain_notice() {
        for os in ["linux", "windows"] {
            let notice = lan_dark_notice(os);
            assert!(!notice.contains("macOS"));
            assert!(notice.contains("multicast does not reach the wire"));
        }
    }

    #[tokio::test]
    async fn the_probe_stops_at_the_first_success() {
        let calls = AtomicU32::new(0);
        let up_on_second = || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move { n == 2 }
        };
        assert!(any_attempt_succeeds(3, Duration::from_millis(1), up_on_second).await);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_dead_wire_exhausts_every_attempt_then_gives_up() {
        let calls = AtomicU32::new(0);
        let never = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { false }
        };
        assert!(!any_attempt_succeeds(3, Duration::from_millis(1), never).await);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
