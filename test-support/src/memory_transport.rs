// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! IN-MEMORY data-plane transport for the tests: a telephone switchboard
//! that routes streams by `node_id`, with no network and no iroh. Two
//! Cores sharing the same `MemorySwitchboard` open streams to each other as
//! two iroh endpoints would — deterministically and instantly.
//!
//! Double of the Core lib's `PeerTransport` (the daemon, for its part, wires
//! up the real iroh impl, compiled natively).

use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::sync::{Arc, Mutex};

use onedevice_core::{HomeRelay, Incoming, IoStream, Opening, PeerAddr, PeerTransport, Reach};
use tokio::io::DuplexStream;
use tokio::sync::mpsc;

/// Buffer of each in-memory pipe. Generous: the tests exchange small
/// messages, never enough to fill it (otherwise a `write_all` before the
/// peer reads would block).
const PIPE_BUF: usize = 64 * 1024;

/// An incoming stream: the caller's `node_id` + the pipe.
type Wire = (String, DuplexStream);

/// An entry in the switchboard: the device's inbox, and the relay it will
/// publish — `open` requires knowing it, like the real iroh.
struct Route {
    relay_url: Option<String>,
    /// Has this endpoint been asked to make itself reachable (`listen`)? Nothing
    /// here binds anything — there is no socket — but the daemon's transport binds
    /// LAZILY, so "a pairing window makes this device dialable" is a wiring fact
    /// with a real consequence, and one only a double can pin from a test.
    listened: Arc<std::sync::atomic::AtomicBool>,
    /// Announcing on the fake LAN (`join_lan`): visible to the other members
    /// and dialable with no relay — the double of a real endpoint whose mDNS
    /// discovery is on.
    on_lan: bool,
    /// This endpoint's LAN generation sender: bumped when ANY membership
    /// changes, because anyone's join or leave may change what this one sees.
    lan_gen: tokio::sync::watch::Sender<u64>,
    /// What this endpoint claims about itself first-hand (`own_reach`): the
    /// addresses a caller may PRESENT to reach it off the LAN, and the relay
    /// hint it will sign. Empty at birth (a real endpoint has observed
    /// nothing either), and declared by the test (`declare_reach`), which is
    /// the double of the transport watching its own addresses move.
    reach: Reach,
    /// Bumped when `reach` changes: the Core's reach watcher re-pulls.
    reach_gen: tokio::sync::watch::Sender<u64>,
    /// Every word `announce_relays` handed this endpoint, in order - the
    /// relay list and, since #88, the announced payload cap riding with it:
    /// the double records what the real transport would fold into its next
    /// bind (#105), so a test can assert both the content and that an
    /// unchanged announcement was not re-pushed.
    announced: Vec<(Vec<String>, Option<u64>)>,
    /// The payload cap this endpoint ENFORCES on its sized opens - the double
    /// of #88's "the announced relays are the effective source and they are
    /// rendezvous-only above the cap". A test knob (`set_payload_cap`),
    /// deliberately separate from the `announced` recording above: whether a
    /// heard word becomes an enforced one is the daemon's derivation
    /// (`resolve_relay`), proven by the daemon's own tests.
    payload_cap: Option<u64>,
    tx: mpsc::UnboundedSender<Wire>,
}

/// The shared switchboard: `node_id` → the inbox of that device's transport.
/// Clonable — the endpoints share it.
#[derive(Clone, Default)]
pub struct MemorySwitchboard {
    routes: Arc<Mutex<HashMap<String, Route>>>,
}

impl MemorySwitchboard {
    pub fn new() -> MemorySwitchboard {
        MemorySwitchboard::default()
    }

    /// Creates a device's transport, registered under its `node_id`. `relay_url`
    /// is what the Core will publish in the directory — and what a caller must
    /// PRESENT to reach it: the real iroh impl cannot connect without the
    /// published relay (or LAN visibility, below), and neither can the fake. A
    /// more permissive fake would make tests pass on a directory state in
    /// which the real one would never connect. Off the LAN at birth — like a
    /// real endpoint, whose mDNS is a separate switch.
    pub fn endpoint(
        &self,
        node_id: impl Into<String>,
        relay_url: Option<String>,
    ) -> Arc<MemoryTransport> {
        let node_id = node_id.into();
        let (tx, rx) = mpsc::unbounded_channel();
        let lan_gen = tokio::sync::watch::channel(0).0;
        let reach_gen = tokio::sync::watch::channel(0).0;
        let listened = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.routes.lock().unwrap().insert(
            node_id.clone(),
            Route {
                relay_url: relay_url.clone(),
                listened: listened.clone(),
                on_lan: false,
                lan_gen: lan_gen.clone(),
                reach: Reach::default(),
                reach_gen: reach_gen.clone(),
                announced: Vec::new(),
                payload_cap: None,
                tx,
            },
        );
        Arc::new(MemoryTransport {
            node_id,
            relay_url,
            listened,
            lan_gen,
            reach_gen,
            switchboard: self.clone(),
            inbox: tokio::sync::Mutex::new(rx),
        })
    }

    /// Declares what an endpoint knows first-hand about how it can be dialed:
    /// the double of the real transport watching its addresses move (a network
    /// joined, a VPN up). The addresses become presentable in `open` (a caller
    /// must hold one to reach the device off the LAN), and the endpoint's own
    /// Core hears the change through `reach_changes`. An unknown `node_id` is a
    /// test bug, so it panics.
    pub fn declare_reach(&self, node_id: &str, reach: Reach) {
        let mut routes = self.routes.lock().unwrap();
        let route = routes
            .get_mut(node_id)
            .expect("declare_reach: node_id unknown to the switchboard");
        if route.reach == reach {
            return;
        }
        route.reach = reach;
        route.reach_gen.send_modify(|generation| *generation += 1);
    }

    /// The relay lists `announce_relays` handed to this endpoint so far,
    /// oldest first. An unknown `node_id` is a test bug, so it panics.
    pub fn announced(&self, node_id: &str) -> Vec<Vec<String>> {
        self.announced_words(node_id)
            .into_iter()
            .map(|(relays, _)| relays)
            .collect()
    }

    /// The whole words `announce_relays` handed to this endpoint so far -
    /// each relay list with the payload cap that rode with it (#88), oldest
    /// first. An unknown `node_id` is a test bug, so it panics.
    pub fn announced_words(&self, node_id: &str) -> Vec<(Vec<String>, Option<u64>)> {
        self.routes
            .lock()
            .unwrap()
            .get(node_id)
            .expect("announced: node_id unknown to the switchboard")
            .announced
            .clone()
    }

    /// Arms the double of the announced relay role (#88) on one endpoint:
    /// its sized opens (`open_for_payload`) above `cap` bytes then require a
    /// DIRECT route - a presented address the peer declared, or the shared
    /// fake LAN - and fail with a `NO_DIRECT_PATH`-prefixed error when only
    /// the relay route would carry them, exactly like the real transport
    /// after a failed hole punch. An unknown `node_id` is a test bug, so it
    /// panics.
    pub fn set_payload_cap(&self, node_id: &str, cap: Option<u64>) {
        self.routes
            .lock()
            .unwrap()
            .get_mut(node_id)
            .expect("set_payload_cap: node_id unknown to the switchboard")
            .payload_cap = cap;
    }

    /// Has this endpoint been asked to make itself reachable? See
    /// [`Route::listened`]. An unknown `node_id` is a test bug, so it panics.
    pub fn listened(&self, node_id: &str) -> bool {
        self.routes
            .lock()
            .unwrap()
            .get(node_id)
            .expect("listened: node_id unknown to the switchboard")
            .listened
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Puts a device on the fake LAN: its `node_id` becomes visible to the
    /// other members (`lan_peers`) and members can open streams to it with no
    /// relay — the double of mDNS discovery, announcement and resolution
    /// bundled like the real `lan_discovery` flag. An unknown `node_id` is a
    /// test bug, so it panics.
    pub fn join_lan(&self, node_id: &str) {
        self.set_lan(node_id, true, "join_lan");
    }

    /// Takes a device off the fake LAN (moved away, radio turned off): it
    /// stops being visible and LAN-dialable, its relay — if any — remains.
    pub fn leave_lan(&self, node_id: &str) {
        self.set_lan(node_id, false, "leave_lan");
    }

    fn set_lan(&self, node_id: &str, on_lan: bool, what: &str) {
        let mut routes = self.routes.lock().unwrap();
        let route = routes
            .get_mut(node_id)
            .unwrap_or_else(|| panic!("{what}: node_id unknown to the switchboard"));
        if route.on_lan == on_lan {
            return;
        }
        route.on_lan = on_lan;
        // Everyone's view may have changed: wake every member's watcher —
        // like a real mDNS announcement, heard by the whole network.
        for route in routes.values() {
            route.lan_gen.send_modify(|generation| *generation += 1);
        }
    }
}

pub struct MemoryTransport {
    node_id: String,
    relay_url: Option<String>,
    listened: Arc<std::sync::atomic::AtomicBool>,
    lan_gen: tokio::sync::watch::Sender<u64>,
    reach_gen: tokio::sync::watch::Sender<u64>,
    switchboard: MemorySwitchboard,
    inbox: tokio::sync::Mutex<mpsc::UnboundedReceiver<Wire>>,
}

impl std::fmt::Debug for MemoryTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let short = &self.node_id[..self.node_id.len().min(8)];
        write!(f, "MemoryTransport({short})")
    }
}

impl PeerTransport for MemoryTransport {
    fn announce_relays(&self, relays: &[String], relay_max_payload: Option<u64>) {
        let mut routes = self.switchboard.routes.lock().unwrap();
        let route = routes
            .get_mut(&self.node_id)
            .expect("announce_relays: node_id unknown to the switchboard");
        route.announced.push((relays.to_vec(), relay_max_payload));
    }

    fn open_for_payload<'a>(&'a self, peer: &'a PeerAddr, payload: u64) -> Opening<'a> {
        Box::pin(async move {
            // The double of the enforced role (#88): an over-cap payload
            // needs a direct route. Direct here = a presented address the
            // peer declared, or the shared fake LAN - the relay route is
            // exactly what the cap refuses. Checked on the CALLER's own
            // route entry: the policy is what this device's transport heard
            // and enforces, not a property of the callee.
            //
            // The refusal is minted ONLY when the relay route is the one
            // route there is: the real transport refuses after a connect
            // that succeeded over the relay, so a peer with no route at all
            // must keep failing as unreachable (`open`'s error), never as a
            // policy refusal - the two carry different remedies.
            let over_cap = {
                let routes = self.switchboard.routes.lock().unwrap();
                let cap = routes
                    .get(&self.node_id)
                    .and_then(|route| route.payload_cap);
                cap.is_some_and(|cap| payload > cap)
            };
            if over_cap {
                let relay_only = {
                    let routes = self.switchboard.routes.lock().unwrap();
                    let me_on_lan = routes.get(&self.node_id).is_some_and(|r| r.on_lan);
                    routes.get(&peer.node_id).is_some_and(|route| {
                        let relay_route =
                            route.relay_url.is_some() && peer.relay_url == route.relay_url;
                        let lan_route = me_on_lan && route.on_lan;
                        let addr_route = peer
                            .addrs
                            .iter()
                            .any(|presented| route.reach.addrs.contains(presented));
                        relay_route && !lan_route && !addr_route
                    })
                };
                if relay_only {
                    return Err(Error::other(format!(
                        "{}: the deployment's relays are rendezvous-only and no direct \
                         path formed (in-memory double)",
                        onedevice_core::NO_DIRECT_PATH
                    )));
                }
            }
            self.open(peer).await
        })
    }

    fn open<'a>(&'a self, peer: &'a PeerAddr) -> Opening<'a> {
        Box::pin(async move {
            let (target, registered_relay, registered_addrs, lan_route) = {
                let routes = self.switchboard.routes.lock().unwrap();
                let me_on_lan = routes.get(&self.node_id).is_some_and(|r| r.on_lan);
                match routes.get(&peer.node_id) {
                    Some(route) => (
                        route.tx.clone(),
                        route.relay_url.clone(),
                        route.reach.addrs.clone(),
                        me_on_lan && route.on_lan,
                    ),
                    None => {
                        return Err(Error::new(
                            ErrorKind::ConnectionRefused,
                            format!(
                                "peer unknown to the in-memory switchboard: {}",
                                peer.node_id
                            ),
                        ));
                    }
                }
            };
            // Three routes, like the real impl. The relay: it must be the one
            // the peer published, presented by the caller — a stale one does
            // not connect. The addresses: the caller must PRESENT one the peer
            // actually declared: same discipline, a stale hint dials nothing.
            // The LAN: both endpoints on it: mDNS resolves the peer
            // regardless of what else (if anything) was presented.
            let relay_route = registered_relay.is_some() && peer.relay_url == registered_relay;
            let addr_route = peer
                .addrs
                .iter()
                .any(|presented| registered_addrs.contains(presented));
            if !relay_route && !addr_route && !lan_route {
                return Err(Error::new(
                    ErrorKind::HostUnreachable,
                    format!(
                        "peer {} unreachable: relay presented {:?}, real relay {:?}, \
                         addrs presented {:?}, real addrs {:?}, on the LAN: {lan_route}",
                        peer.node_id,
                        peer.relay_url,
                        registered_relay,
                        peer.addrs,
                        registered_addrs
                    ),
                ));
            }
            let (mine, theirs) = tokio::io::duplex(PIPE_BUF);
            target
                .send((self.node_id.clone(), theirs))
                .map_err(|_| Error::new(ErrorKind::ConnectionReset, "peer disconnected"))?;
            Ok(Box::new(mine) as Box<dyn IoStream>)
        })
    }

    fn accept(&self) -> Incoming<'_> {
        Box::pin(async move {
            let mut inbox = self.inbox.lock().await;
            match inbox.recv().await {
                Some((peer_id, stream)) => Ok((peer_id, Box::new(stream) as Box<dyn IoStream>)),
                None => Err(Error::new(
                    ErrorKind::BrokenPipe,
                    "in-memory switchboard closed",
                )),
            }
        })
    }

    fn home_relay(&self) -> HomeRelay<'_> {
        let relay = self.relay_url.clone();
        Box::pin(async move { relay })
    }

    fn lan_peers(&self) -> Vec<String> {
        let routes = self.switchboard.routes.lock().unwrap();
        // Resolving is part of the same switch as announcing: a device off the
        // LAN hears nobody, exactly like a transport whose mDNS is off.
        if !routes.get(&self.node_id).is_some_and(|r| r.on_lan) {
            return Vec::new();
        }
        routes
            .iter()
            .filter(|(id, route)| route.on_lan && id.as_str() != self.node_id)
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn lan_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.lan_gen.subscribe()
    }

    fn own_reach(&self) -> Option<Reach> {
        // Always a claim: this transport is "bound" from registration, like
        // the doc on the trait says the empty default is for.
        self.switchboard
            .routes
            .lock()
            .unwrap()
            .get(&self.node_id)
            .map(|route| route.reach.clone())
    }

    fn reach_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.reach_gen.subscribe()
    }

    fn listen(&self) -> onedevice_core::Listening<'_> {
        // Nothing to bind: this transport has been reachable since it was
        // registered. Recorded, though — see `Route::listened`.
        self.listened
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(std::future::ready(()))
    }
}
