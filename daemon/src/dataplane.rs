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
//! passive: portmapper probes (UPnP/PCP/NAT-PMP), and with a relay opted in
//! (#104), relay probes every ~20 s and a persistent connection to the
//! elected relay. The device key is only
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
use onedevice_core::{
    ALPN, Closing, HomeRelay, Incoming, IoStream, Listening, Opening, PeerAddr, PeerTransport,
    Reach,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

/// Maximum wait for the endpoint to become reachable via a relay, after which
/// `home_relay` returns `None` (offline, no relay to publish).
const HOME_RELAY_WAIT: Duration = Duration::from_secs(10);

/// The relay setting, three-valued (#104): infrastructure somebody CHOSE.
/// Off is the default; riding n0's public relays is an explicit opt-in, no
/// longer something that happens to an unconfigured device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayChoice {
    /// No relay at all (`RelayMode::Disabled`): no relay connection, no
    /// housekeeping connection, zero unsolicited network contact at boot.
    /// What still connects: the LAN (mDNS), any directly dialable address
    /// (VPN tunnels, the signed hints of #87), and whatever relays the
    /// device's server announces (#105): the announcement fills exactly
    /// this default (`resolve_relay`).
    Off,
    /// The n0 public relays (`RelayMode::Default`), chosen instead of
    /// inherited. The endpoint elects a home relay from the map, and THAT
    /// election becomes the signed `relay_hint`: the opt-in is what makes
    /// the claim honest ("never a relay nobody chose").
    N0,
    /// A specific relay: self-hosted, or an operator's.
    Url(RelayUrl),
}

/// Mode, signed hint and the `home_relay` short-circuit, derived in one
/// place from the local choice and the deployment's announcement, so the
/// signed word can never disagree with how the endpoint runs. The decided
/// precedence (#104/#105): an explicit local relay beats the announcement,
/// which beats off: the announcement only ever fills the off default. Off
/// with no announcement signs nothing and skips the relay probe; a URL
/// signs itself; n0 signs whatever home relay the endpoint elects, and so
/// does an announced list (the user chose the server, the operator chose
/// the relays: the election is chosen all the way down).
fn resolve_relay(relay: RelayChoice, announced: &[RelayUrl]) -> (RelayMode, HintSource, bool) {
    match relay {
        RelayChoice::Off if !announced.is_empty() => (
            RelayMode::custom(announced.iter().cloned()),
            HintSource::Elected,
            false,
        ),
        RelayChoice::Off => (RelayMode::Disabled, HintSource::Fixed(None), true),
        RelayChoice::N0 => (RelayMode::Default, HintSource::Elected, false),
        RelayChoice::Url(url) => {
            let hint = HintSource::Fixed(Some(url.to_string()));
            (RelayMode::custom([url]), hint, false)
        }
    }
}

/// Where `own_reach`'s `relay_hint` comes from. Kept apart from the mode:
/// the hint is a durable signed word, the mode is how the endpoint runs.
enum HintSource {
    /// The configured relay, verbatim (or none): it stands whether or not
    /// the endpoint is currently connected to it.
    Fixed(Option<String>),
    /// The elected home relay, followed as the election moves (the same
    /// watcher that follows the addresses re-signs it). Two modes elect: the
    /// explicit n0 opt-in, and the off default filled by a server
    /// announcement (#105); either way the election is a relay somebody
    /// chose, which is what lets it be signed.
    Elected,
}

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
    /// The socket addresses the endpoint currently claims (loopback and
    /// unspecified filtered out), maintained by `reach_task` from iroh's own
    /// watcher: the `addrs` half of `own_reach`.
    addrs: Arc<Mutex<Vec<String>>>,
    /// The `relay_hint` half of `own_reach`: a relay somebody CHOSE, and only
    /// that (#89, amended by #104). The configured relay verbatim in URL
    /// mode; in n0 mode the elected home relay, maintained by `reach_task`
    /// (the opt-in is the choice, the election is its current shape); `None`
    /// in off mode, always.
    relay_hint: Arc<Mutex<Option<String>>>,
    /// Off mode: `home_relay` answers `None` immediately instead of waiting
    /// out `HOME_RELAY_WAIT` on an `online()` that can never resolve (iroh
    /// pends forever with no relay configured).
    relay_off: bool,
    /// Bumped by `reach_task` at every actual address change; `reach_changes`
    /// subscribes to it. Same ownership story as `lan_gen`.
    reach_gen: tokio::sync::watch::Sender<u64>,
    /// Folds iroh's `watch_addr` stream into `addrs`. Aborted at drop (iroh's
    /// watcher outlives `close` by design: it only ends with the last
    /// `Endpoint` clone).
    reach_task: tokio::task::JoinHandle<()>,
}

impl Drop for IrohTransport {
    fn drop(&mut self) {
        self.acceptor.abort();
        self.reach_task.abort();
        for task in [&self.lan_task, &self.lan_probe].into_iter().flatten() {
            task.abort();
        }
    }
}

/// The mDNS service name endpoints announce themselves under
/// (`<node_id>._1device._udp.local`). Ours rather than the crate's
/// default `irohv1`: only 1Device devices answer each other, and a
/// packet capture names the protocol honestly.
const MDNS_SERVICE: &str = "1device";

impl IrohTransport {
    /// Production endpoint. `relay`: the three-valued choice (#104): off by
    /// default (no relay, no housekeeping connection), n0 or a URL as an
    /// explicit opt-in. `announced`: the deployment's relay announcement
    /// (#105), folded in under the local choice by `resolve_relay`.
    /// Certificates verified normally, no DNS discovery. `lan_discovery`
    /// adds the mDNS lookup (see the module header): resolution AND
    /// announcement: one flag, both directions, because announcing without
    /// resolving (or the reverse) would just be a device its siblings
    /// half-see.
    pub async fn bind(
        seed: [u8; 32],
        relay: RelayChoice,
        announced: Vec<RelayUrl>,
        lan_discovery: bool,
    ) -> anyhow::Result<IrohTransport> {
        let (lan_gen, _) = tokio::sync::watch::channel(0);
        let (reach_gen, _) = tokio::sync::watch::channel(0);
        Self::bind_with_gen(seed, relay, announced, lan_discovery, lan_gen, reach_gen).await
    }

    /// `bind`, with the LAN and reach generation channels supplied by the
    /// caller: `LazyIrohTransport` needs `lan_changes`/`reach_changes` to
    /// answer BEFORE the endpoint exists, so it owns the senders and hands
    /// them over at the (lazy) bind.
    pub async fn bind_with_gen(
        seed: [u8; 32],
        relay: RelayChoice,
        announced: Vec<RelayUrl>,
        lan_discovery: bool,
        lan_gen: tokio::sync::watch::Sender<u64>,
        reach_gen: tokio::sync::watch::Sender<u64>,
    ) -> anyhow::Result<IrohTransport> {
        let secret = SecretKey::from_bytes(&seed);
        let mdns = lan_lookup(&secret, lan_discovery);
        let (relay_mode, hint, relay_off) = resolve_relay(relay, &announced);
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(relay_mode);
        if let Some(mdns) = &mdns {
            builder = builder.address_lookup(mdns.clone());
        }
        Self::finish(builder, mdns, lan_gen, hint, reach_gen, relay_off).await
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
        // The test map's relays ARE the configured ones, same word as
        // `RelayMode::custom` in `bind_with_gen`.
        let hint = HintSource::Fixed(relay_map.urls::<Vec<_>>().first().map(ToString::to_string));
        Self::bind_test_with(
            seed,
            RelayMode::Custom(relay_map),
            hint,
            false,
            lan_discovery,
        )
        .await
    }

    /// Test endpoint in ELECTED-hint mode: the n0 opt-in's machinery
    /// (`RelayMode::Default` electing a home relay, the election signed as the
    /// hint) run against a LOCAL relay map instead of n0's production one, so
    /// the tests prove the election-follows-the-watcher plumbing offline.
    #[cfg(feature = "test-utils")]
    pub async fn bind_test_elected(
        seed: [u8; 32],
        relay_map: iroh::RelayMap,
        lan_discovery: bool,
    ) -> anyhow::Result<IrohTransport> {
        Self::bind_test_with(
            seed,
            RelayMode::Custom(relay_map),
            HintSource::Elected,
            false,
            lan_discovery,
        )
        .await
    }

    /// Test endpoint in OFF mode: the production default (#104), THROUGH the
    /// production mapping (`resolve_relay`), so the LAN and address-hint
    /// tests prove the routes the off default is documented to keep against
    /// the very derivation the daemon runs.
    #[cfg(feature = "test-utils")]
    pub async fn bind_test_off(
        seed: [u8; 32],
        lan_discovery: bool,
    ) -> anyhow::Result<IrohTransport> {
        let (relay_mode, hint, relay_off) = resolve_relay(RelayChoice::Off, &[]);
        Self::bind_test_with(seed, relay_mode, hint, relay_off, lan_discovery).await
    }

    #[cfg(feature = "test-utils")]
    async fn bind_test_with(
        seed: [u8; 32],
        relay_mode: RelayMode,
        hint: HintSource,
        relay_off: bool,
        lan_discovery: bool,
    ) -> anyhow::Result<IrohTransport> {
        let secret = SecretKey::from_bytes(&seed);
        let mdns = lan_lookup(&secret, lan_discovery);
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(relay_mode)
            .portmapper_config(iroh::endpoint::PortmapperConfig::Disabled)
            .ca_tls_config(iroh::tls::CaTlsConfig::insecure_skip_verify());
        if let Some(mdns) = &mdns {
            builder = builder.address_lookup(mdns.clone());
        }
        let (lan_gen, _) = tokio::sync::watch::channel(0);
        let (reach_gen, _) = tokio::sync::watch::channel(0);
        Self::finish(builder, mdns, lan_gen, hint, reach_gen, relay_off).await
    }

    async fn finish(
        builder: iroh::endpoint::Builder,
        mdns: Option<MdnsAddressLookup>,
        lan_gen: tokio::sync::watch::Sender<u64>,
        hint: HintSource,
        reach_gen: tokio::sync::watch::Sender<u64>,
        relay_off: bool,
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
        let addrs = Arc::new(Mutex::new(Vec::new()));
        let (relay_hint, follow_election) = match hint {
            HintSource::Fixed(url) => (Arc::new(Mutex::new(url)), false),
            HintSource::Elected => (Arc::new(Mutex::new(None)), true),
        };
        let reach_task = tokio::spawn(watch_reach(
            endpoint.clone(),
            addrs.clone(),
            relay_hint.clone(),
            follow_election,
            reach_gen.clone(),
        ));
        Ok(IrohTransport {
            endpoint,
            ready: tokio::sync::Mutex::new(rx),
            acceptor,
            lan,
            lan_gen,
            lan_task,
            lan_probe,
            addrs,
            relay_hint,
            relay_off,
            reach_gen,
            reach_task,
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

/// Whether multicast comes back at all: something looped, whatever route it
/// took. Public because it is also the tests' judge of whether an environment
/// can host the real-socket LAN tests at all: GitHub's hosted macOS runners
/// refuse the send outright (five identical timeouts in a row on every try,
/// while real Macs pass).
pub async fn multicast_reaches_the_wire() -> bool {
    !matches!(multicast_loopback().await, Beacon::Nothing)
}

/// What one probe attempt observed.
enum Beacon {
    /// The beacon looped back, stamped with this source address: the address
    /// of the interface the kernel sent it out of. That choice is the whole
    /// diagnosis, because a VPN that captured multicast routing stamps the
    /// tunnel's address on a beacon the actual network never saw.
    Heard(std::net::IpAddr),
    /// Nothing came back within the timeout.
    Nothing,
}

/// The raw probe: a beacon sent to the mDNS group must come back to a member
/// of that group on the same host. It uses the real group but an ephemeral
/// port, so it never collides with mDNS itself.
async fn multicast_loopback() -> Beacon {
    use std::net::Ipv4Addr;

    let group = Ipv4Addr::new(224, 0, 0, 251);
    let Ok(rx) = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else {
        return Beacon::Nothing;
    };
    let Ok(local) = rx.local_addr() else {
        return Beacon::Nothing;
    };
    if rx.join_multicast_v4(group, Ipv4Addr::UNSPECIFIED).is_err() {
        return Beacon::Nothing;
    }
    let Ok(tx) = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else {
        return Beacon::Nothing;
    };
    let beacon = b"1device multicast probe";
    // Two beacons: the first can race the group join on a slow stack.
    for _ in 0..2 {
        let _ = tx.send_to(beacon, (group, local.port())).await;
        let mut buf = [0u8; 64];
        match tokio::time::timeout(Duration::from_secs(2), rx.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) if &buf[..n] == beacon => return Beacon::Heard(from.ip()),
            _ => {}
        }
    }
    Beacon::Nothing
}

/// One plain line when LAN discovery is on but the wire is dark or captured.
/// Without it, the only trace is the discovery library's per-send warnings:
/// hundreds of lines that name no cure. Two distinct diagnoses share the
/// probe:
///
/// - **dark**: nothing loops back at all. macOS is the expected culprit: it
///   asks each fresh build for the "Local Network" permission and quietly
///   refuses every multicast send (`No route to host`) until someone answers,
///   so its line names the switch to flip.
/// - **tunneled**: the beacon loops back, but stamped with a VPN tunnel's
///   address. The kernel routed multicast into the tunnel (per-app VPNs on
///   Android and desktop VPNs holding the default route both do this), looped
///   a copy back locally, and the actual network never saw a byte: a deafness
///   a naive probe files as a healthy wire. The line names the tunnel and the
///   way out.
///
/// Three probes over twenty seconds ride out a Wi-Fi still associating at
/// login and a permission prompt just answered.
async fn warn_if_lan_dark() {
    match worst_that_stands(3, Duration::from_secs(10), wire_verdict).await {
        Wire::Healthy => {}
        Wire::Tunneled(iface) => tracing::warn!("{}", lan_tunneled_notice(&iface)),
        Wire::Dark => tracing::warn!("{}", lan_dark_notice(std::env::consts::OS)),
    }
}

/// One probe attempt's verdict on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wire {
    /// The beacon looped back from a non-tunnel interface: multicast gets out.
    Healthy,
    /// The beacon looped back from this tunnel interface: a VPN swallows it.
    Tunneled(String),
    /// The beacon never looped back at all.
    Dark,
}

/// One probe attempt, classified against the live interface table.
async fn wire_verdict() -> Wire {
    match multicast_loopback().await {
        Beacon::Nothing => Wire::Dark,
        Beacon::Heard(source) => match tunnel_owning_source(source, &interface_summaries()) {
            Some(iface) => Wire::Tunneled(iface),
            None => Wire::Healthy,
        },
    }
}

/// Runs `probe` up to `attempts` times, `pause` apart, stopping at the first
/// healthy attempt. What stands at the end is the most *specific* bad news
/// seen: a tunneled loop explains a dark wire (the VPN that swallowed one
/// beacon swallowed the others), so it wins over plain darkness.
async fn worst_that_stands<F>(attempts: u32, pause: Duration, probe: impl Fn() -> F) -> Wire
where
    F: std::future::Future<Output = Wire>,
{
    let mut tunneled = None;
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(pause).await;
        }
        match probe().await {
            Wire::Healthy => return Wire::Healthy,
            Wire::Tunneled(iface) => tunneled = Some(iface),
            Wire::Dark => {}
        }
    }
    match tunneled {
        Some(iface) => Wire::Tunneled(iface),
        None => Wire::Dark,
    }
}

/// The line for a dark wire, per platform. Takes the OS as a value so every
/// platform's message is checkable from any platform.
fn lan_dark_notice(os: &str) -> &'static str {
    match os {
        "macos" => {
            "LAN discovery is on, but multicast does not reach the wire — macOS is \
             likely denying local network access: allow 1Device under System \
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

/// The line for a captured wire: the beacon does loop back, but through a
/// VPN. Names the tunnel so the user knows which app to open, and the
/// remedies in the order they usually exist.
fn lan_tunneled_notice(iface: &str) -> String {
    format!(
        "LAN discovery is on, but multicast is routed into a VPN tunnel \
         ({iface}) instead of your network: this device neither sees nor is \
         seen on its own network. Exclude 1Device from the VPN (WireGuard \
         calls it \"Excluded applications\") or allow LAN traffic in the \
         VPN's settings; server and relay are unaffected."
    )
}

/// The slice of an interface the tunnel verdict needs, split from `netdev`'s
/// full struct so the verdict stays a pure function the tests can feed.
struct InterfaceSummary {
    /// What the warning will name: the friendly name where the platform has
    /// one (Windows kernel names are GUIDs), the kernel name elsewhere.
    name: String,
    /// Every IPv4 address the interface owns.
    addrs: Vec<std::net::Ipv4Addr>,
    /// Tunnel-shaped, per [`looks_like_tunnel`].
    tunnel: bool,
}

/// The live interface table, reduced to what the verdict reads.
fn interface_summaries() -> Vec<InterfaceSummary> {
    netdev::get_interfaces()
        .into_iter()
        .map(|iface| InterfaceSummary {
            tunnel: looks_like_tunnel(&iface),
            addrs: iface.ipv4_addrs(),
            name: match &iface.friendly_name {
                Some(friendly) => friendly.clone(),
                None => iface.name.clone(),
            },
        })
        .collect()
}

/// Tunnel-shaped: the flag heuristic (up, point-to-point, no broadcast)
/// covers tun/utun/wg out of the box, the OS-reported type covers Windows
/// tunnel adapters, and the names catch what both miss (wintun registers as
/// an ordinary virtual adapter).
fn looks_like_tunnel(iface: &netdev::Interface) -> bool {
    iface.is_tun()
        || iface.if_type == netdev::interface::types::InterfaceType::Tunnel
        || name_says_tunnel(
            &iface.name,
            iface.friendly_name.as_deref(),
            iface.description.as_deref(),
        )
}

/// Name-based recognition, the third net. Kernel names are terse and
/// prefix-stable (`tun0`, `utun3`, `wg0`, `tailscale0`); friendly names and
/// descriptions (Windows, mostly) are prose, matched on the vendors and the
/// word itself. `eth0`, `en0`, `wlan0` and "Wi-Fi" match nothing here: a
/// wrong accusation would be worse than the old silence.
fn name_says_tunnel(name: &str, friendly: Option<&str>, description: Option<&str>) -> bool {
    const NAME_PREFIXES: &[&str] = &[
        "tun",
        "tap",
        "utun",
        "wg",
        "zt",
        "ppp",
        "ipsec",
        "nordlynx",
        "tailscale",
    ];
    const PROSE_KEYWORDS: &[&str] = &[
        "vpn",
        "tunnel",
        "wintun",
        "tap-windows",
        "wireguard",
        "tailscale",
        "zerotier",
        "openvpn",
        "nordlynx",
        "proton",
    ];
    let name = name.to_ascii_lowercase();
    NAME_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || [friendly, description]
            .into_iter()
            .flatten()
            .map(str::to_ascii_lowercase)
            .any(|text| PROSE_KEYWORDS.iter().any(|word| text.contains(word)))
}

/// The tunnel owning the looped beacon's source address, if that is where it
/// came from. `None` for a source no interface claims is deliberate: without
/// an owner there is no diagnosis, and the benefit of the doubt keeps the
/// old behavior (a heard beacon, a silent probe). The lan-dark hunt already
/// convicted an innocent VPN once; this function is written to never repeat
/// that.
fn tunnel_owning_source(
    source: std::net::IpAddr,
    interfaces: &[InterfaceSummary],
) -> Option<String> {
    let std::net::IpAddr::V4(source) = source else {
        return None;
    };
    interfaces
        .iter()
        .find(|iface| iface.addrs.contains(&source))
        .filter(|iface| iface.tunnel)
        .map(|iface| iface.name.clone())
}

/// Maintains this endpoint's own claimed addresses from iroh's `watch_addr`
/// watcher: every direct address the endpoint currently stands behind (bound
/// interface addresses, observed external ones), as text, minus what no
/// sibling could ever dial (loopback, unspecified). Sorted, so "changed" means
/// changed: iroh may re-emit the same set in a different order.
///
/// With `follow_election` (the n0 opt-in, #104), the SAME watcher also
/// maintains `relay_hint`: `watch_addr` re-emits when the endpoint elects or
/// changes its home relay, and the election is the current shape of the
/// choice the opt-in made, re-signed like an address change, by the same
/// wake. In fixed-hint mode the mutex is never touched here.
///
/// Ends when the LAST clone of the endpoint drops (iroh's watcher outlives
/// `close` by design); the abort at the transport's drop is the real
/// terminator.
async fn watch_reach(
    endpoint: Endpoint,
    addrs: Arc<Mutex<Vec<String>>>,
    relay_hint: Arc<Mutex<Option<String>>>,
    follow_election: bool,
    reach_gen: tokio::sync::watch::Sender<u64>,
) {
    let mut watcher = iroh::Watcher::stream(endpoint.watch_addr());
    while let Some(claim) = watcher.next().await {
        let mut fresh: Vec<String> = claim
            .ip_addrs()
            .filter(|sa| !sa.ip().is_loopback() && !sa.ip().is_unspecified())
            .map(|sa| sa.to_string())
            .collect();
        fresh.sort();
        fresh.dedup();
        // Mutate, release, THEN bump, same discipline as the LAN set: a
        // watcher woken by the bump re-pulls and must never observe the
        // pre-mutation state.
        let addr_count = fresh.len();
        let mut changed = update_if_changed(&addrs, fresh);
        if changed {
            tracing::debug!(addrs = addr_count, "own direct addresses changed");
        }
        if follow_election {
            let elected = claim.relay_urls().next().map(ToString::to_string);
            let stands = elected.is_some();
            if update_if_changed(&relay_hint, elected) {
                tracing::debug!(elected = stands, "elected home relay changed");
                changed = true;
            }
        }
        if changed {
            reach_gen.send_modify(|generation| *generation += 1);
        }
    }
}

/// Replaces `slot` when `fresh` differs, saying whether it did: the compare
/// half of the mutate-release-THEN-bump discipline, shared by the address
/// set and the elected hint so neither half of the claim can drift from it.
/// The lock is released before the caller bumps.
fn update_if_changed<T: PartialEq>(slot: &Mutex<T>, fresh: T) -> bool {
    let mut held = slot.lock().expect("lock reach slot");
    if *held == fresh {
        false
    } else {
        *held = fresh;
        true
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

/// A peer's iroh address, built from its directory entry. The signed address
/// hints ride along as dial candidates: iroh tries them for a direct
/// connection, in parallel with the relay and whatever mDNS resolves, and the
/// handshake authenticates whoever answers. A hint that does not parse is
/// skipped, not fatal: it is a hint, and the other routes stand.
fn peer_to_addr(peer: &PeerAddr) -> io::Result<EndpointAddr> {
    let bytes: [u8; 32] = hex::decode(&peer.node_id)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "node_id hex of 32 bytes"))?;
    let id = PublicKey::from_bytes(&bytes).map_err(|e| wrap("invalid node_id", e))?;
    let mut addr = EndpointAddr::new(id);
    // The relay is skipped like a hint when it does not parse, not fatal:
    // failing the whole dial over one garbled route would let it mask every
    // good one (the signed addresses, mDNS).
    if let Some(relay) = &peer.relay_url {
        match relay.parse::<RelayUrl>() {
            Ok(relay) => addr = addr.with_relay_url(relay),
            Err(e) => tracing::debug!(relay = %relay, error = %e, "unparseable relay: skipped"),
        }
    }
    for hint in &peer.addrs {
        match hint.parse::<std::net::SocketAddr>() {
            Ok(socket) => addr = addr.with_ip_addr(socket),
            Err(e) => {
                tracing::debug!(hint = %hint, error = %e, "unparseable address hint: skipped")
            }
        }
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
            // Off mode: no relay will ever be elected, and `online()` would
            // pend the full budget for nothing. Session establishment calls
            // this, so answering now is what keeps the off default free of
            // a ten-second stall at every login.
            if self.relay_off {
                return None;
            }
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

    fn own_reach(&self) -> Option<Reach> {
        Some(Reach {
            addrs: self.addrs.lock().expect("lock own addrs").clone(),
            relay_hint: self.relay_hint.lock().expect("lock relay hint").clone(),
        })
    }

    fn reach_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.reach_gen.subscribe()
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
    relay: RelayChoice,
    /// Read once, at the bind — like `relay`, a change requires a Core
    /// restart.
    lan_discovery: bool,
    /// The deployment's announced relays (#105), parsed and deduplicated:
    /// handed over by the Core (`announce_relays`) from its disk cache at
    /// startup and from the descriptor at each session establishment. Read
    /// at the bind, where `resolve_relay` folds it in under the local
    /// choice; a change that lands after the bind applies at the next
    /// start, and says so.
    announced: Mutex<Vec<RelayUrl>>,
    /// What the bind actually consumed, so a post-bind announcement change
    /// is told apart from a repeat of the same list.
    bound_announced: Mutex<Option<Vec<RelayUrl>>>,
    /// The LAN generation channel, created HERE so `lan_changes` can hand out
    /// receivers before the endpoint exists: the sender is given to the inner
    /// transport at the (lazy) bind, and a receiver taken while still unbound
    /// simply waits — the sender never drops.
    lan_gen: tokio::sync::watch::Sender<u64>,
    /// Same story for the reach generation channel (`reach_changes`).
    reach_gen: tokio::sync::watch::Sender<u64>,
    cell: tokio::sync::OnceCell<IrohTransport>,
    /// Wakes the waiting `accept`s once the endpoint is bound.
    bound: tokio::sync::Notify,
}

impl LazyIrohTransport {
    pub fn new(config_dir: PathBuf, relay: RelayChoice, lan_discovery: bool) -> LazyIrohTransport {
        LazyIrohTransport {
            config_dir,
            relay,
            lan_discovery,
            announced: Mutex::new(Vec::new()),
            bound_announced: Mutex::new(None),
            lan_gen: tokio::sync::watch::channel(0).0,
            reach_gen: tokio::sync::watch::channel(0).0,
            cell: tokio::sync::OnceCell::new(),
            bound: tokio::sync::Notify::new(),
        }
    }

    /// The endpoint, bound on the first call. A failure does not poison the
    /// cell: the next call retries.
    async fn ensure(&self) -> io::Result<&IrohTransport> {
        let first = self.cell.get().is_none();
        let transport = self
            .cell
            .get_or_try_init(|| async {
                // The device key is read HERE, on the first use — never before
                // the Core's instance lock.
                let seed = onedevice_core::load_or_generate_device_seed(&self.config_dir)
                    .map_err(|e| wrap("device identity", format!("{e:#}")))?;
                // The announcement as it stands at THIS bind: what the fleet
                // heard from its operator, folded in under the local choice.
                let announced = self.announced.lock().expect("lock announced").clone();
                let transport = IrohTransport::bind_with_gen(
                    seed,
                    self.relay.clone(),
                    announced.clone(),
                    self.lan_discovery,
                    self.lan_gen.clone(),
                    self.reach_gen.clone(),
                )
                .await
                .map_err(|e| wrap("binding the iroh endpoint", format!("{e:#}")))?;
                *self.bound_announced.lock().expect("lock bound announced") = Some(announced.clone());
                // An announcement that landed DURING the bind saw
                // `bound_announced` still unset and said nothing; the bind
                // consumed the older list, so the late word gets its line
                // here instead of vanishing.
                let current = self.announced.lock().expect("lock announced").clone();
                if announcement_outran_by_the_bind(&self.relay, Some(&announced), &current) {
                    tracing::info!(
                        "the deployment's relay announcement changed during the bind: applied at the next Core start"
                    );
                }
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
        // The bind itself is a reach event: the inner watcher's first bump can
        // land while the cell is still unset, in which case the Core's watcher
        // woke, read "no claim", and consumed the generation. Bumping once
        // here (only by whoever raced the init, so hot callers stay silent)
        // re-arms it now that `own_reach` answers.
        if first && self.cell.get().is_some() {
            self.reach_gen.send_modify(|generation| *generation += 1);
        }
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

    fn own_reach(&self) -> Option<Reach> {
        // Not bound = NO claim, and it must stay that way (hot path, must
        // never trigger the bind). Not even the configured relay: an unbound
        // endpoint is not connected to it, so claiming it would be a stale
        // hint, and answering "empty" instead of "nothing" would have the
        // reach watcher wipe, at every boot, the hints the record signed
        // last session, when the machine may not have moved at all.
        self.cell.get().and_then(PeerTransport::own_reach)
    }

    fn reach_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        // Same sender the inner transport bumps once bound.
        self.reach_gen.subscribe()
    }

    fn announce_relays(&self, relays: &[String]) {
        // Parsed here, at the seam where strings become routes, with the
        // same tolerance as `peer_to_addr`: a garbled entry is skipped, not
        // fatal, so one bad URL in an announcement cannot mask the good
        // ones. Deduplicated so a sloppy announcement costs nothing.
        let mut seen = HashSet::new();
        let parsed: Vec<RelayUrl> = relays
            .iter()
            .filter_map(|raw| match raw.parse::<RelayUrl>() {
                Ok(url) => Some(url),
                Err(e) => {
                    tracing::debug!(relay = %raw, error = %e, "unparseable announced relay: skipped");
                    None
                }
            })
            .filter(|url| seen.insert(url.to_string()))
            .collect();
        *self.announced.lock().expect("lock announced") = parsed.clone();
        let bound = self.bound_announced.lock().expect("lock bound announced");
        if announcement_outran_by_the_bind(&self.relay, bound.as_deref(), &parsed) {
            tracing::info!(
                "the deployment's relay announcement changed: applied at the next Core start"
            );
        }
    }
}

/// Whether a fresh announcement arrived too late for the running endpoint:
/// only the off default consumes the announcement (precedence: the local
/// word wins), so only there is a post-bind change worth a line, and only
/// when the bind consumed something else. `bound` is what the bind actually
/// used (`None`: not bound yet, the fresh word will be consumed normally).
fn announcement_outran_by_the_bind(
    relay: &RelayChoice,
    bound: Option<&[RelayUrl]>,
    fresh: &[RelayUrl],
) -> bool {
    matches!(relay, RelayChoice::Off) && bound.is_some_and(|used| used != fresh)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn custom_urls(mode: RelayMode) -> Vec<String> {
        match mode {
            RelayMode::Custom(map) => map
                .urls::<Vec<_>>()
                .iter()
                .map(ToString::to_string)
                .collect(),
            other => panic!("expected a custom relay map, not {other:?}"),
        }
    }

    #[test]
    fn resolve_relay_maps_each_choice_to_its_mode_hint_and_probe() {
        // Off, the default: Disabled (no housekeeping connection), nothing
        // signed, and the home_relay probe short-circuited. This is the
        // production arm the daemon and the phone actually run; a regression
        // here silently puts fresh installs back on relays nobody chose.
        let (mode, hint, off) = resolve_relay(RelayChoice::Off, &[]);
        assert!(matches!(mode, RelayMode::Disabled));
        assert!(matches!(hint, HintSource::Fixed(None)));
        assert!(off, "off must skip the home_relay wait");

        // n0: the stock map, and the ELECTION as the signed word.
        let (mode, hint, off) = resolve_relay(RelayChoice::N0, &[]);
        assert!(matches!(mode, RelayMode::Default));
        assert!(
            matches!(hint, HintSource::Elected),
            "the opt-in signs its election"
        );
        assert!(!off);

        // A URL: a one-relay map, the URL itself as the signed word.
        let url: RelayUrl = "https://relay.example".parse().expect("relay url");
        let (mode, hint, off) = resolve_relay(RelayChoice::Url(url.clone()), &[]);
        assert_eq!(custom_urls(mode), vec![url.to_string()]);
        assert!(matches!(hint, HintSource::Fixed(Some(u)) if u == url.to_string()));
        assert!(!off);
    }

    #[test]
    fn the_announcement_fills_the_off_default_and_never_beats_the_local_word() {
        let eu: RelayUrl = "https://relay-eu.example".parse().expect("relay url");
        let us: RelayUrl = "https://relay-us.example".parse().expect("relay url");
        let announced = vec![eu.clone(), us.clone()];

        // Off + an announcement: the whole list becomes the map, the
        // election becomes the signed word (chosen all the way down: the
        // user chose the server, the operator chose the relays), and the
        // home_relay probe runs.
        let (mode, hint, off) = resolve_relay(RelayChoice::Off, &announced);
        assert_eq!(custom_urls(mode), vec![eu.to_string(), us.to_string()]);
        assert!(matches!(hint, HintSource::Elected));
        assert!(!off);

        // The precedence (#104): an explicit local relay beats the
        // announcement.
        let local: RelayUrl = "https://relay.mine.example".parse().expect("relay url");
        let (mode, hint, _) = resolve_relay(RelayChoice::Url(local.clone()), &announced);
        assert_eq!(custom_urls(mode), vec![local.to_string()]);
        assert!(matches!(hint, HintSource::Fixed(Some(u)) if u == local.to_string()));
        let (mode, _, _) = resolve_relay(RelayChoice::N0, &announced);
        assert!(matches!(mode, RelayMode::Default));
    }

    #[test]
    fn announced_relays_are_parsed_deduplicated_and_stored_for_the_bind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lazy = LazyIrohTransport::new(dir.path().to_path_buf(), RelayChoice::Off, false);
        PeerTransport::announce_relays(
            &lazy,
            &[
                "https://relay-eu.example".to_string(),
                "not a url".to_string(),
                "https://relay-eu.example".to_string(),
            ],
        );
        let held: Vec<String> = lazy
            .announced
            .lock()
            .expect("lock announced")
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(held, vec!["https://relay-eu.example/".to_string()]);
        // Hearing an announcement is not a use: nothing may bind for it.
        assert!(
            !dir.path().join("device.key").exists(),
            "announcing must not bind"
        );
    }

    #[test]
    fn a_late_announcement_is_reported_only_when_the_bind_consumed_another() {
        let eu: RelayUrl = "https://relay-eu.example".parse().expect("relay url");
        let us: RelayUrl = "https://relay-us.example".parse().expect("relay url");

        // Not bound yet: the fresh word will be consumed normally, no line.
        assert!(!announcement_outran_by_the_bind(
            &RelayChoice::Off,
            None,
            std::slice::from_ref(&eu)
        ));
        // Bound with the same list: a repeat, no line.
        assert!(!announcement_outran_by_the_bind(
            &RelayChoice::Off,
            Some(std::slice::from_ref(&eu)),
            std::slice::from_ref(&eu)
        ));
        // Bound with another list: the word arrived too late, say so.
        assert!(announcement_outran_by_the_bind(
            &RelayChoice::Off,
            Some(std::slice::from_ref(&eu)),
            std::slice::from_ref(&us)
        ));
        // A local choice never consumes the announcement: never a line.
        assert!(!announcement_outran_by_the_bind(
            &RelayChoice::N0,
            Some(std::slice::from_ref(&eu)),
            std::slice::from_ref(&us)
        ));
    }

    #[test]
    fn update_if_changed_reports_only_actual_changes() {
        let slot = Mutex::new(Some("https://a.example".to_string()));
        // Same value: no report, no wasted re-signing wakeup.
        assert!(!update_if_changed(
            &slot,
            Some("https://a.example".to_string())
        ));
        // A different value lands and reports: the elected-hint half of the
        // claim (and the address half) both ride exactly this.
        assert!(update_if_changed(
            &slot,
            Some("https://b.example".to_string())
        ));
        assert_eq!(
            *slot.lock().expect("slot"),
            Some("https://b.example".to_string())
        );
        // Withdrawal is a change too (a home relay lost is a claim to drop).
        assert!(update_if_changed(&slot, None));
        assert_eq!(*slot.lock().expect("slot"), None);
    }

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
    async fn the_probe_stops_at_the_first_healthy_attempt() {
        let calls = AtomicU32::new(0);
        let healthy_on_second = || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move { if n == 2 { Wire::Healthy } else { Wire::Dark } }
        };
        let verdict = worst_that_stands(3, Duration::from_millis(1), healthy_on_second).await;
        assert_eq!(verdict, Wire::Healthy);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_dead_wire_exhausts_every_attempt_then_gives_up() {
        let calls = AtomicU32::new(0);
        let never = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Wire::Dark }
        };
        let verdict = worst_that_stands(3, Duration::from_millis(1), never).await;
        assert_eq!(verdict, Wire::Dark);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_tunneled_loop_outlives_later_dark_attempts() {
        // The VPN that swallowed the first beacon swallowed the next ones
        // too: the specific diagnosis must not decay into the generic one.
        let calls = AtomicU32::new(0);
        let tunneled_then_dark = || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n == 1 {
                    Wire::Tunneled("wg0".into())
                } else {
                    Wire::Dark
                }
            }
        };
        let verdict = worst_that_stands(3, Duration::from_millis(1), tunneled_then_dark).await;
        assert_eq!(verdict, Wire::Tunneled("wg0".into()));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_healthy_attempt_clears_an_earlier_tunneled_one() {
        // A VPN disconnecting mid-probe: the wire is fine now, stay silent.
        let calls = AtomicU32::new(0);
        let tunneled_then_healthy = || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n == 1 {
                    Wire::Tunneled("wg0".into())
                } else {
                    Wire::Healthy
                }
            }
        };
        let verdict = worst_that_stands(3, Duration::from_millis(1), tunneled_then_healthy).await;
        assert_eq!(verdict, Wire::Healthy);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    fn summary(name: &str, tunnel: bool, addrs: &[std::net::Ipv4Addr]) -> InterfaceSummary {
        InterfaceSummary {
            name: name.into(),
            addrs: addrs.to_vec(),
            tunnel,
        }
    }

    #[test]
    fn a_beacon_from_a_tunnel_address_names_the_tunnel() {
        let interfaces = [
            summary("eth0", false, &["192.168.1.34".parse().unwrap()]),
            summary("wg0", true, &["10.8.0.2".parse().unwrap()]),
        ];
        let verdict = tunnel_owning_source("10.8.0.2".parse().unwrap(), &interfaces);
        assert_eq!(verdict.as_deref(), Some("wg0"));
    }

    #[test]
    fn a_beacon_from_the_lan_interface_accuses_nobody() {
        let interfaces = [
            summary("eth0", false, &["192.168.1.34".parse().unwrap()]),
            summary("wg0", true, &["10.8.0.2".parse().unwrap()]),
        ];
        assert_eq!(
            tunnel_owning_source("192.168.1.34".parse().unwrap(), &interfaces),
            None
        );
    }

    #[test]
    fn an_unclaimed_source_gets_the_benefit_of_the_doubt() {
        let interfaces = [summary("wg0", true, &["10.8.0.2".parse().unwrap()])];
        assert_eq!(
            tunnel_owning_source("172.16.9.9".parse().unwrap(), &interfaces),
            None
        );
        assert_eq!(
            tunnel_owning_source("fe80::1".parse().unwrap(), &interfaces),
            None
        );
    }

    #[test]
    fn kernel_tunnel_names_are_recognized() {
        for name in [
            "tun0",
            "utun3",
            "wg0",
            "tailscale0",
            "ppp0",
            "nordlynx",
            "zt7nnig34",
        ] {
            assert!(
                name_says_tunnel(name, None, None),
                "{name} should read as a tunnel"
            );
        }
        for name in ["eth0", "en0", "wlan0", "enp3s0", "br-lan", "docker0"] {
            assert!(
                !name_says_tunnel(name, None, None),
                "{name} must not read as a tunnel"
            );
        }
    }

    #[test]
    fn windows_prose_names_are_recognized() {
        // On Windows the kernel name is a GUID; the giveaway is the friendly
        // name or the adapter description.
        let guid = "{7D3A2F41-9C58-4E06-B2D1-5A8C39E01B67}";
        assert!(name_says_tunnel(guid, Some("WireGuard Tunnel"), None));
        assert!(name_says_tunnel(
            guid,
            Some("Ethernet 2"),
            Some("TAP-Windows Adapter V9")
        ));
        assert!(name_says_tunnel(guid, Some("ProtonVPN"), None));
        assert!(!name_says_tunnel(
            guid,
            Some("Wi-Fi"),
            Some("Intel(R) Wi-Fi 6 AX201")
        ));
        assert!(!name_says_tunnel(
            guid,
            Some("Ethernet"),
            Some("Realtek PCIe GbE")
        ));
    }

    #[test]
    fn the_tunneled_notice_names_the_interface_and_the_way_out() {
        let notice = lan_tunneled_notice("wg0");
        assert!(notice.contains("wg0"));
        assert!(notice.contains("Exclude 1Device from the VPN"));
        assert!(notice.contains("server and relay are unaffected"));
    }
}
