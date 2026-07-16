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
    /// PRESENT to reach it: the real iroh impl (no discovery,
    /// `presets::Minimal`) cannot connect without the published relay, and
    /// neither can the fake. A more permissive fake would make tests pass on a
    /// directory state in which the real one would never connect.
    pub fn endpoint(
        &self,
        node_id: impl Into<String>,
        relay_url: Option<String>,
    ) -> Arc<MemoryTransport> {
        let node_id = node_id.into();
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes.lock().unwrap().insert(
            node_id.clone(),
            Route {
                relay_url: relay_url.clone(),
                tx,
            },
        );
        Arc::new(MemoryTransport {
            node_id,
            relay_url,
            switchboard: self.clone(),
            inbox: tokio::sync::Mutex::new(rx),
        })
    }
}

pub struct MemoryTransport {
    node_id: String,
    relay_url: Option<String>,
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
            let (target, registered_relay) = {
                let routes = self.switchboard.routes.lock().unwrap();
                match routes.get(&peer.node_id) {
                    Some(route) => (route.tx.clone(), route.relay_url.clone()),
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
            // Same requirement as the real iroh impl: without the relay the
            // peer published, no route (no fallback discovery). A stale
            // relay does not connect either.
            if registered_relay.is_none() || peer.relay_url != registered_relay {
                return Err(Error::new(
                    ErrorKind::HostUnreachable,
                    format!(
                        "peer {} unreachable: relay presented {:?}, real relay {:?}",
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
}
