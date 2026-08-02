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

use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use universallink_core::{HomeRelay, Incoming, IoStream, Opening, PeerAddr, PeerTransport};

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
    /// Announcing on the fake LAN (`join_lan`): visible to the other members
    /// and dialable with no relay — the double of a real endpoint whose mDNS
    /// discovery is on.
    on_lan: bool,
    /// This endpoint's LAN generation sender: bumped when ANY membership
    /// changes, because anyone's join or leave may change what this one sees.
    lan_gen: tokio::sync::watch::Sender<u64>,
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
        self.routes.lock().unwrap().insert(
            node_id.clone(),
            Route {
                relay_url: relay_url.clone(),
                on_lan: false,
                lan_gen: lan_gen.clone(),
                tx,
            },
        );
        Arc::new(MemoryTransport {
            node_id,
            relay_url,
            lan_gen,
            switchboard: self.clone(),
            inbox: tokio::sync::Mutex::new(rx),
        })
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
    lan_gen: tokio::sync::watch::Sender<u64>,
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
    fn open<'a>(&'a self, peer: &'a PeerAddr) -> Opening<'a> {
        Box::pin(async move {
            let (target, registered_relay, lan_route) = {
                let routes = self.switchboard.routes.lock().unwrap();
                let me_on_lan = routes.get(&self.node_id).is_some_and(|r| r.on_lan);
                match routes.get(&peer.node_id) {
                    Some(route) => (
                        route.tx.clone(),
                        route.relay_url.clone(),
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
            // Two routes, like the real impl. The relay: it must be the one
            // the peer published, presented by the caller — a stale one does
            // not connect. The LAN: both endpoints on it — mDNS resolves the
            // peer regardless of what relay (if any) was presented.
            let relay_route = registered_relay.is_some() && peer.relay_url == registered_relay;
            if !relay_route && !lan_route {
                return Err(Error::new(
                    ErrorKind::HostUnreachable,
                    format!(
                        "peer {} unreachable: relay presented {:?}, real relay {:?}, on the LAN: {lan_route}",
                        peer.node_id, peer.relay_url, registered_relay
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
}
