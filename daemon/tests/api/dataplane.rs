// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Two REAL iroh endpoints establish a connection via a local relay (offline,
//! deterministic — portmapper turned off by `bind_test`) and run THE Core's
//! transfer protocol (`send_transfer` / `read_offer`+`receive_bodies`), exactly
//! as the `serve` loop and `files.send` serve it in production. This is the
//! proof that the in-memory pipe cannot give: the real QUIC lifecycle — a
//! responder that drops the connection too soon would abandon the
//! acknowledgment in flight (implicit close(0)), and the protocol must survive
//! it.

use std::time::Duration;

use iroh::test_utils::run_relay_server;
use iroh::{RelayUrl, SecretKey};
use onedevice_core::{
    OutgoingFile, PeerAddr, PeerTransport, read_offer, receive_bodies, send_transfer,
};
use onedevice_daemon::dataplane::{IrohTransport, LazyIrohTransport, multicast_reaches_the_wire};
use tokio::time::timeout;

fn node_id(seed: &[u8; 32]) -> String {
    hex::encode(SecretKey::from_bytes(seed).public().as_bytes())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_core_transfer_protocol_survives_real_quic() {
    let (relay_map, relay_url, _guard) = run_relay_server().await.expect("local relay");

    let seed_a = [1u8; 32];
    let seed_b = [2u8; 32];
    let a = IrohTransport::bind_test(seed_a, relay_map.clone(), false)
        .await
        .expect("endpoint A");
    let b = IrohTransport::bind_test(seed_b, relay_map, false)
        .await
        .expect("endpoint B");

    // Reachable via the relay before connecting: with neither discovery nor a
    // direct address, the relay is B's only route to A.
    timeout(Duration::from_secs(15), async {
        tokio::join!(a.endpoint().online(), b.endpoint().online());
    })
    .await
    .expect("endpoints online via the relay");

    let peer = PeerAddr {
        node_id: node_id(&seed_a),
        relay_url: Some(relay_url.to_string()),
        addrs: Vec::new(),
    };

    // Content larger than one chunk: the bodies are streamed, not framed.
    let contents = vec![42u8; 200_000];
    let src_dir = tempfile::tempdir().expect("tempdir source");
    let src = src_dir.path().join("payload.bin");
    std::fs::write(&src, &contents).expect("write the source");
    let dest_dir = tempfile::tempdir().expect("tempdir dest");

    let written = timeout(Duration::from_secs(20), async {
        let respond = async {
            let (peer_id, mut stream) = a.accept().await.expect("accept A");
            // The identity returned by `accept` is half the contract: it is
            // what the `serve` loop matches against the account's directory.
            assert_eq!(peer_id, node_id(&seed_b), "incoming peer's identity");
            // The PRODUCTION functions, as-is — it is up to them to hold the
            // connection until the acknowledgment, without a test crutch.
            let manifest = read_offer(&mut stream).await.expect("offer");
            receive_bodies(&mut stream, dest_dir.path(), &manifest, &mut |_, _| {})
                .await
                .expect("receive")
        };
        let ask = async {
            let files = vec![OutgoingFile {
                name: "payload.bin".into(),
                source: Some(src.clone()),
                size: contents.len() as u64,
                is_dir: false,
            }];
            let mut stream = b.open(&peer).await.expect("open B->A");
            send_transfer(&mut stream, &files, &mut |_, _| {})
                .await
                .expect("send");
        };
        let (written, ()) = tokio::join!(respond, ask);
        written
    })
    .await
    .expect("transfer within the deadline");

    assert_eq!(written.len(), 1);
    assert_eq!(std::fs::read(&written[0]).expect("received file"), contents);
    // Graceful shutdown (through the trait, like the binary on shutdown):
    // otherwise iroh logs an endpoint abandonment.
    a.close().await;
    b.close().await;
}

/// Two endpoints with NO relay at all (empty map): mDNS is the only possible
/// route, so a successful transfer proves the LAN discovery end to end —
/// announcement on one side, resolution on the other, direct connection by
/// `node_id` alone. This is what a `PeerAddr` without `relay_url` will become
/// once the Core stops requiring a published relay (next block).
#[tokio::test(flavor = "multi_thread")]
async fn two_endpoints_reach_each_other_over_the_lan_without_any_relay() {
    if !multicast_reaches_the_wire().await {
        eprintln!("skipped: this environment does not route multicast");
        return;
    }
    let seed_a = [3u8; 32];
    let seed_b = [4u8; 32];
    let a = IrohTransport::bind_test(seed_a, iroh::RelayMap::empty(), true)
        .await
        .expect("endpoint A");
    let b = IrohTransport::bind_test(seed_b, iroh::RelayMap::empty(), true)
        .await
        .expect("endpoint B");

    // No relay to publish, and that must not block anything: reachability
    // comes from the mDNS announcement, not from `online()`.
    let peer = PeerAddr {
        node_id: node_id(&seed_a),
        relay_url: None,
        addrs: Vec::new(),
    };

    let contents = b"across the room, not the internet".to_vec();
    let src_dir = tempfile::tempdir().expect("tempdir source");
    let src = src_dir.path().join("note.txt");
    std::fs::write(&src, &contents).expect("write the source");
    let dest_dir = tempfile::tempdir().expect("tempdir dest");

    // Generous budget: the mDNS announcement is periodic, and the lookup can
    // wait for the next beacon before it resolves.
    let written = timeout(Duration::from_secs(30), async {
        let respond = async {
            let (peer_id, mut stream) = a.accept().await.expect("accept A");
            assert_eq!(peer_id, node_id(&seed_b), "incoming peer's identity");
            let manifest = read_offer(&mut stream).await.expect("offer");
            receive_bodies(&mut stream, dest_dir.path(), &manifest, &mut |_, _| {})
                .await
                .expect("receive")
        };
        let ask = async {
            let files = vec![OutgoingFile {
                name: "note.txt".into(),
                source: Some(src.clone()),
                size: contents.len() as u64,
                is_dir: false,
            }];
            let mut stream = b.open(&peer).await.expect("open B->A over the LAN");
            send_transfer(&mut stream, &files, &mut |_, _| {})
                .await
                .expect("send");
        };
        let (written, ()) = tokio::join!(respond, ask);
        written
    })
    .await
    .expect("LAN transfer within the deadline");

    assert_eq!(written.len(), 1);
    assert_eq!(std::fs::read(&written[0]).expect("received file"), contents);
    a.close().await;
    b.close().await;
}

/// What `lan_peers` reports is what mDNS actually heard: two endpoints with
/// the discovery on end up in each other's set — symmetrically — with no
/// relay involved. This is the daemon half of the Core's reachability gate.
#[tokio::test(flavor = "multi_thread")]
async fn the_lan_set_reflects_what_mdns_hears() {
    if !multicast_reaches_the_wire().await {
        eprintln!("skipped: this environment does not route multicast");
        return;
    }
    let seed_a = [5u8; 32];
    let seed_b = [6u8; 32];
    let a = IrohTransport::bind_test(seed_a, iroh::RelayMap::empty(), true)
        .await
        .expect("endpoint A");
    let b = IrohTransport::bind_test(seed_b, iroh::RelayMap::empty(), true)
        .await
        .expect("endpoint B");

    // Announcements are periodic: poll until both sides hear each other.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let a_sees = a.lan_peers().contains(&node_id(&seed_b));
        let b_sees = b.lan_peers().contains(&node_id(&seed_a));
        if a_sees && b_sees {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "mDNS never surfaced the peers: A sees B: {a_sees}, B sees A: {b_sees}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    a.close().await;
    b.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_endpoint_publishes_its_relay() {
    let (relay_map, relay_url, _guard) = run_relay_server().await.expect("local relay");
    let a = IrohTransport::bind_test([7u8; 32], relay_map, false)
        .await
        .expect("endpoint");

    let home = timeout(Duration::from_secs(15), a.home_relay())
        .await
        .expect("home_relay within the deadline");
    let home: RelayUrl = home.expect("a published relay").parse().expect("relay url");
    assert_eq!(home, relay_url);
    a.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lazy_transport_stays_silent_until_first_use() {
    let dir = tempfile::tempdir().expect("tempdir");
    let transport = LazyIrohTransport::new(dir.path().to_path_buf(), None, false);

    // `accept` is not a use: the Core's `serve` loop calls it right from
    // startup, and a never-enrolled Core must neither read/create `device.key`
    // nor emit a single iroh packet (relay probes, portmapper).
    let pending = timeout(Duration::from_millis(300), transport.accept()).await;
    assert!(
        pending.is_err(),
        "accept must not resolve without a binding"
    );
    assert!(
        !dir.path().join("device.key").exists(),
        "no identity as long as nothing uses the data plane"
    );
    // And closing a transport that was never bound is a non-event.
    transport.close().await;
}

/// A signed address hint is a route of its own: two endpoints with NO relay
/// and NO discovery (nothing but an explicit `addrs` entry on the dial)
/// still connect over real sockets and run the production transfer protocol.
/// This is the daemon half of the off-LAN brick (#87): the Core learns the
/// hints from the gossip, this proves iroh actually dials them.
#[tokio::test(flavor = "multi_thread")]
async fn a_direct_address_hint_dials_with_no_relay_and_no_discovery() {
    let seed_a = [8u8; 32];
    let seed_b = [9u8; 32];
    let a = IrohTransport::bind_test(seed_a, iroh::RelayMap::empty(), false)
        .await
        .expect("endpoint A");
    let b = IrohTransport::bind_test(seed_b, iroh::RelayMap::empty(), false)
        .await
        .expect("endpoint B");

    // Where A actually listens, as a sibling's directory would carry it: its
    // bound sockets, the unspecified host replaced by the loopback the test
    // dials (a real record carries the interface addresses instead: same
    // shape, same dial path).
    let addrs: Vec<String> = a
        .endpoint()
        .bound_sockets()
        .into_iter()
        .map(|socket| {
            if socket.ip().is_unspecified() {
                let localhost = match socket {
                    std::net::SocketAddr::V4(_) => "127.0.0.1".parse().expect("v4"),
                    std::net::SocketAddr::V6(_) => "::1".parse().expect("v6"),
                };
                std::net::SocketAddr::new(localhost, socket.port()).to_string()
            } else {
                socket.to_string()
            }
        })
        .collect();
    assert!(!addrs.is_empty(), "a bound endpoint has sockets");
    let peer = PeerAddr {
        node_id: node_id(&seed_a),
        relay_url: None,
        addrs,
    };

    let contents = b"dialed by hint, nothing else existed".to_vec();
    let src_dir = tempfile::tempdir().expect("tempdir source");
    let src = src_dir.path().join("hint.txt");
    std::fs::write(&src, &contents).expect("write the source");
    let dest_dir = tempfile::tempdir().expect("tempdir dest");

    let written = timeout(Duration::from_secs(20), async {
        let respond = async {
            let (peer_id, mut stream) = a.accept().await.expect("accept A");
            assert_eq!(peer_id, node_id(&seed_b), "incoming peer's identity");
            let manifest = read_offer(&mut stream).await.expect("offer");
            receive_bodies(&mut stream, dest_dir.path(), &manifest, &mut |_, _| {})
                .await
                .expect("receive")
        };
        let ask = async {
            let files = vec![OutgoingFile {
                name: "hint.txt".into(),
                source: Some(src.clone()),
                size: contents.len() as u64,
                is_dir: false,
            }];
            let mut stream = b.open(&peer).await.expect("open B->A by address hint");
            send_transfer(&mut stream, &files, &mut |_, _| {})
                .await
                .expect("send");
        };
        let (written, ()) = tokio::join!(respond, ask);
        written
    })
    .await
    .expect("hint-dialed transfer within the deadline");

    assert_eq!(written.len(), 1);
    assert_eq!(std::fs::read(&written[0]).expect("received file"), contents);
    a.close().await;
    b.close().await;
}

/// What `own_reach` claims as a relay is the CONFIGURED one, and only that
/// (#89: relay use is explicit, never a silent default). And an UNBOUND lazy
/// transport claims nothing at all, config included: no claim is not an empty
/// claim: answering "empty" would have the reach watcher wipe the hints a
/// record signed last session, at every boot, before anything binds.
#[tokio::test(flavor = "multi_thread")]
async fn own_reach_claims_the_configured_relay_and_only_that() {
    // Unbound: NO claim, and asking must not bind.
    let dir = tempfile::tempdir().expect("tempdir");
    let url: RelayUrl = "https://relay.self-hosted.example"
        .parse()
        .expect("relay url");
    let lazy = LazyIrohTransport::new(dir.path().to_path_buf(), Some(url), false);
    assert_eq!(lazy.own_reach(), None, "unbound is silent");
    assert!(
        !dir.path().join("device.key").exists(),
        "asking must not bind"
    );

    // Not configured: no relay claimed, a bound endpoint included, however
    // real the relays it might elect by default for the server path.
    let bare = IrohTransport::bind_test([10u8; 32], iroh::RelayMap::empty(), false)
        .await
        .expect("endpoint");
    assert_eq!(bare.own_reach().expect("bound claims").relay_hint, None);
    bare.close().await;

    // Configured on a REAL endpoint: the claim is the config, verbatim.
    let (relay_map, relay_url, _guard) = run_relay_server().await.expect("local relay");
    let configured = IrohTransport::bind_test([11u8; 32], relay_map, false)
        .await
        .expect("endpoint");
    let claimed: RelayUrl = configured
        .own_reach()
        .expect("bound claims")
        .relay_hint
        .expect("the configured relay")
        .parse()
        .expect("relay url");
    assert_eq!(claimed, relay_url);
    configured.close().await;
}
