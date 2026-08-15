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
use onedevice_daemon::dataplane::{
    IrohTransport, LazyIrohTransport, RelayChoice, multicast_reaches_the_wire,
};
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

/// The announced role (#88) against REAL iroh endpoints: both bound in the
/// production announced shape (`resolve_relay`: off default filled by the
/// deployment's word, rendezvous-only above a cap), an OVER-CAP sized open
/// waits for the punched direct path and then proceeds - on localhost the
/// punch always lands, so this pins that the enforcement never
/// false-refuses a pair that can meet directly, over the real QUIC
/// lifecycle. (The refusal itself needs a pair that genuinely cannot
/// hole-punch, which no offline test can stage: the surfacing of the code
/// is proven against the in-memory double in the Core suite, and the real
/// refusal belongs to live validation.)
#[tokio::test(flavor = "multi_thread")]
async fn an_over_cap_open_proceeds_once_the_punch_lands() {
    let (_relay_map, relay_url, _guard) = run_relay_server().await.expect("local relay");

    let seed_a = [11u8; 32];
    let seed_b = [12u8; 32];
    let announced = vec![relay_url.clone()];
    let a = IrohTransport::bind_test_announced(seed_a, announced.clone(), Some(1024), false)
        .await
        .expect("endpoint A");
    let b = IrohTransport::bind_test_announced(seed_b, announced, Some(1024), false)
        .await
        .expect("endpoint B");

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

    // Well over the cap: the sized open must hold the stream back until the
    // direct path is the selected one, then let the whole transfer through.
    let contents = vec![7u8; 200_000];
    let src_dir = tempfile::tempdir().expect("tempdir source");
    let src = src_dir.path().join("payload.bin");
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
                name: "payload.bin".into(),
                source: Some(src.clone()),
                size: contents.len() as u64,
                is_dir: false,
            }];
            let mut stream = b
                .open_for_payload(&peer, contents.len() as u64)
                .await
                .expect("sized open B->A over the cap");
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
    a.close().await;
    b.close().await;
}

/// Two endpoints in OFF mode (`RelayMode::Disabled`, the production default,
/// #104): mDNS is the only possible route, so a successful transfer proves
/// the LAN discovery end to end (announcement on one side, resolution on the
/// other, direct connection by `node_id` alone) and with it the off
/// default's promise that the LAN needs no relay.
#[tokio::test(flavor = "multi_thread")]
async fn two_endpoints_reach_each_other_over_the_lan_without_any_relay() {
    if !multicast_reaches_the_wire().await {
        eprintln!("skipped: this environment does not route multicast");
        return;
    }
    let seed_a = [3u8; 32];
    let seed_b = [4u8; 32];
    let a = IrohTransport::bind_test_off(seed_a, true)
        .await
        .expect("endpoint A");
    let b = IrohTransport::bind_test_off(seed_b, true)
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
    let a = IrohTransport::bind_test_off(seed_a, true)
        .await
        .expect("endpoint A");
    let b = IrohTransport::bind_test_off(seed_b, true)
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
    let transport = LazyIrohTransport::new(dir.path().to_path_buf(), RelayChoice::Off, false);

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
    let a = IrohTransport::bind_test_off(seed_a, false)
        .await
        .expect("endpoint A");
    let b = IrohTransport::bind_test_off(seed_b, false)
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

/// What `own_reach` claims as a relay is one somebody CHOSE, and only that
/// (#89, amended by #104). And an UNBOUND lazy transport claims nothing at
/// all, config included: no claim is not an empty claim: answering "empty"
/// would have the reach watcher wipe the hints a record signed last session,
/// at every boot, before anything binds.
#[tokio::test(flavor = "multi_thread")]
async fn own_reach_claims_the_chosen_relay_and_only_that() {
    // Unbound: NO claim, and asking must not bind.
    let dir = tempfile::tempdir().expect("tempdir");
    let url: RelayUrl = "https://relay.self-hosted.example"
        .parse()
        .expect("relay url");
    let lazy = LazyIrohTransport::new(dir.path().to_path_buf(), RelayChoice::Url(url), false);
    assert_eq!(lazy.own_reach(), None, "unbound is silent");
    assert!(
        !dir.path().join("device.key").exists(),
        "asking must not bind"
    );

    // Off (the default): no relay claimed, a bound endpoint included.
    let bare = IrohTransport::bind_test_off([10u8; 32], false)
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

/// The off default answers the session's relay probe NOW: `home_relay` must
/// not sit out its ten-second budget on an `online()` that can never resolve
/// without a relay (#104): that stall would tax every login of exactly the
/// deployments the default is for.
#[tokio::test(flavor = "multi_thread")]
async fn the_off_default_publishes_no_relay_and_does_not_stall() {
    let a = IrohTransport::bind_test_off([12u8; 32], false)
        .await
        .expect("endpoint");
    let home = timeout(Duration::from_secs(3), a.home_relay())
        .await
        .expect("home_relay must answer immediately in off mode");
    assert_eq!(home, None, "no relay to publish");
    a.close().await;
}

/// The n0 opt-in signs the ELECTED home relay (#104): with the election run
/// against a local relay map, `own_reach` ends up claiming the elected URL:
/// the amended invariant ("never a relay nobody chose") lets an explicit
/// opt-in stand for the relay it elects, re-signed by the same watcher that
/// re-signs addresses.
#[tokio::test(flavor = "multi_thread")]
async fn the_n0_opt_in_signs_the_elected_home_relay() {
    let (relay_map, relay_url, _guard) = run_relay_server().await.expect("local relay");
    let a = IrohTransport::bind_test_elected([13u8; 32], relay_map, false)
        .await
        .expect("endpoint");

    // The election takes a moment: follow the transport's own wake signal,
    // exactly as the Core's reach watcher does.
    let mut changes = a.reach_changes();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let hint = a.own_reach().expect("bound claims").relay_hint;
        if let Some(hint) = hint {
            let hint: RelayUrl = hint.parse().expect("relay url");
            assert_eq!(hint, relay_url, "the elected home relay is the claim");
            break;
        }
        let woke = tokio::time::timeout_at(deadline, changes.changed()).await;
        assert!(woke.is_ok(), "no home relay elected within the deadline");
        woke.expect("deadline").expect("watcher alive");
    }
    a.close().await;
}
