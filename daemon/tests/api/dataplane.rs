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
use tokio::time::timeout;
use universallink_core::{
    OutgoingFile, PeerAddr, PeerTransport, read_offer, receive_bodies, send_transfer,
};
use universallink_daemon::dataplane::{IrohTransport, LazyIrohTransport};

fn node_id(seed: &[u8; 32]) -> String {
    hex::encode(SecretKey::from_bytes(seed).public().as_bytes())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_core_transfer_protocol_survives_real_quic() {
    let (relay_map, relay_url, _guard) = run_relay_server().await.expect("local relay");

    let seed_a = [1u8; 32];
    let seed_b = [2u8; 32];
    let a = IrohTransport::bind_test(seed_a, relay_map.clone())
        .await
        .expect("endpoint A");
    let b = IrohTransport::bind_test(seed_b, relay_map)
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
                source: src.clone(),
                size: contents.len() as u64,
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

#[tokio::test(flavor = "multi_thread")]
async fn an_endpoint_publishes_its_relay() {
    let (relay_map, relay_url, _guard) = run_relay_server().await.expect("local relay");
    let a = IrohTransport::bind_test([7u8; 32], relay_map)
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
    let transport = LazyIrohTransport::new(dir.path().to_path_buf(), None);

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
