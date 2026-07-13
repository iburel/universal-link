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
//! provider).
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

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Context as _;
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, RelayUrl, SecretKey};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use universallink_core::{
    ALPN, Closing, HomeRelay, Incoming, IoStream, Opening, PeerAddr, PeerTransport,
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
}

impl Drop for IrohTransport {
    fn drop(&mut self) {
        self.acceptor.abort();
    }
}

impl IrohTransport {
    /// Production endpoint. `relay`: the deployment's relay (self-hosted) if it
    /// is configured, otherwise the n0 public relays — a server of one's own
    /// must not structurally depend on third-party infra. Certificates
    /// verified normally, no DNS discovery.
    pub async fn bind(seed: [u8; 32], relay: Option<RelayUrl>) -> anyhow::Result<IrohTransport> {
        let relay_mode = match relay {
            Some(url) => RelayMode::custom([url]),
            None => RelayMode::Default,
        };
        let builder = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&seed))
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(relay_mode);
        Self::finish(builder).await
    }

    /// Test endpoint: a LOCAL relay (self-signed certificate) whose
    /// verification we skip, and the portmapper turned off (no UPnP/PCP/NAT-PMP
    /// probes to the test machine's gateway — the tests declare themselves
    /// offline, and they are). Gated by the `test-utils` feature (enabled by
    /// the dev-dependencies only): the unverified TLS path DOES NOT EXIST in
    /// the production binary — the compiler guarantees it, not a convention.
    #[cfg(feature = "test-utils")]
    pub async fn bind_test(
        seed: [u8; 32],
        relay_map: iroh::RelayMap,
    ) -> anyhow::Result<IrohTransport> {
        let builder = Endpoint::builder(presets::Minimal)
            .secret_key(SecretKey::from_bytes(&seed))
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(RelayMode::Custom(relay_map))
            .portmapper_config(iroh::endpoint::PortmapperConfig::Disabled)
            .ca_tls_config(iroh::tls::CaTlsConfig::insecure_skip_verify());
        Self::finish(builder).await
    }

    async fn finish(builder: iroh::endpoint::Builder) -> anyhow::Result<IrohTransport> {
        let endpoint = builder.bind().await.context("binding the iroh endpoint")?;
        let (tx, rx) = mpsc::channel(READY_QUEUE);
        let acceptor = tokio::spawn(acceptor(endpoint.clone(), tx));
        Ok(IrohTransport {
            endpoint,
            ready: tokio::sync::Mutex::new(rx),
            acceptor,
        })
    }

    /// The underlying endpoint (local address, `online()`, for tests).
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
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
    cell: tokio::sync::OnceCell<IrohTransport>,
    /// Wakes the waiting `accept`s once the endpoint is bound.
    bound: tokio::sync::Notify,
}

impl LazyIrohTransport {
    pub fn new(config_dir: PathBuf, relay: Option<RelayUrl>) -> LazyIrohTransport {
        LazyIrohTransport {
            config_dir,
            relay,
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
                let transport = IrohTransport::bind(seed, self.relay.clone())
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

    fn close(&self) -> Closing<'_> {
        Box::pin(async move {
            if let Some(transport) = self.cell.get() {
                transport.close().await;
            }
        })
    }
}
