// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! A live channel between same-role components on two devices (doc/core-api.md,
//! "peers.*"; ticket #124).
//!
//! `peers.channel { device_id }` mints a single-use `channel_token`; the bearer
//! opens a second connection to the socket, presents it, and the connection
//! becomes an opaque framed pipe joined to the far device's own. The Core
//! carries the bytes and reads none of them, so what these tests pin is
//! everything AROUND the bytes: the targeting refusals, the caps, and the
//! moments a channel must die, each with the reason the local component is
//! told, because a pipe that goes mute is the failure mode this primitive
//! exists to avoid.

use std::time::Duration;

use onedevice_core::PeerTransport;
use onedevice_test_support::memory_transport::MemorySwitchboard;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::clipboard_net::RawPeer;
use crate::support::*;

/// A component that may open and receive peer channels.
async fn channeller(core: &TestCore, name: &str, role: &str) -> TestComponent {
    spawn_component(
        core,
        name,
        role,
        &["peers.channel", "devices.read", "session.read"],
    )
    .await
}

/// A GUI-shaped component: what a test needs to log out or strike a device off.
async fn manager(core: &TestCore) -> TestComponent {
    spawn_component(
        core,
        "gui",
        "gui",
        &[
            "session.manage",
            "session.read",
            "devices.read",
            "devices.manage",
        ],
    )
    .await
}

/// A connected pair, each with a `custom`-role channeller that sees the other.
async fn channel_pair(server: &TestServer) -> (TestCore, TestComponent, TestCore, TestComponent) {
    let (a, b) = TestCore::start_pair(server).await;
    let mut ca = channeller(&a, "engine", "custom").await;
    let mut cb = channeller(&b, "engine", "custom").await;
    wait_server_connected(&mut ca, true).await;
    wait_server_connected(&mut cb, true).await;
    wait_reachable(&mut ca, b.device_id()).await;
    wait_reachable(&mut cb, a.device_id()).await;
    (a, ca, b, cb)
}

/// Opens a channel from A to B and takes it at both ends: the two live pipes.
///
/// The two halves are raced on purpose. `peers.channel` does not return until
/// the far end has ATTACHED (that is what makes `accepted` worth more than "a
/// notification was queued"), so a test that called it and only then went
/// looking for the offer would deadlock on itself.
async fn joined(
    a: &TestCore,
    ca: &mut TestComponent,
    b: &TestCore,
    cb: &mut TestComponent,
) -> (PeerPipe, PeerPipe) {
    let (opened, far) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": b.device_id() })),
        async {
            let offer = cb.wait_notification("peer.channel").await;
            assert_eq!(offer["device_id"], json!(a.device_id()));
            b.open_peer_pipe(offer["channel_token"].as_str().expect("channel_token"))
                .await
        }
    );
    let token = opened.expect("peers.channel")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string();
    (a.open_peer_pipe(&token).await, far)
}

/// What a `peer.channel_closed` said, for the component that owned the local
/// end. The budget is the caller's: most reasons are instant, the idle sweep is
/// a Core-side budget elapsing.
async fn closed_reason(c: &mut TestComponent, budget: Duration) -> String {
    let closed = c
        .wait_notification_within("peer.channel_closed", budget)
        .await;
    closed["reason"]
        .as_str()
        .expect("a reason on peer.channel_closed")
        .to_string()
}

// ---------------------------------------------------------------------------
// The pipe itself.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_channel_joins_the_same_role_on_two_devices() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (mut pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;

    // Opaque and verbatim, in both directions: arbitrary bytes, not JSON.
    pa.send(&[0x00, 0x01, 0xff, 0x7f]).await;
    assert_eq!(
        pb.recv().await.as_deref(),
        Some(&[0x00, 0x01, 0xff, 0x7f][..])
    );
    pb.send(b"up").await;
    assert_eq!(pa.recv().await.as_deref(), Some(&b"up"[..]));

    // A zero-length frame is legal: that is what a keepalive looks like to a
    // Core that does not know what a keepalive is.
    pb.send(&[]).await;
    assert_eq!(pa.recv().await.as_deref(), Some(&[][..]));

    // Exactly at the 1 KiB cap: carried. (Above it, below.)
    let full = vec![0x5a; 1024];
    pa.send(&full).await;
    assert_eq!(pb.recv().await, Some(full));

    // Order is preserved, which is what a flow of numbered positions needs.
    for n in 0u8..16 {
        pa.send(&[n]).await;
    }
    for n in 0u8..16 {
        assert_eq!(pb.recv().await.as_deref(), Some(&[n][..]));
    }
    // Nothing was said on the control plane: the Core interpreted none of it.
    ca.assert_silent().await;
    cb.assert_silent().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_candidate_is_offered_the_channel_and_the_first_to_attach_wins() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    // A second holder of the same role and scope on B: it is offered its OWN
    // token, because a token is bound to one component and the offer cannot be
    // the single broadcast frame `peer.message` gets away with.
    let mut second = channeller(&b, "engine-bis", "custom").await;

    let (opened, winner, loser_token) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": b.device_id() })),
        async {
            let offer = cb.wait_notification("peer.channel").await;
            let token = offer["channel_token"]
                .as_str()
                .expect("channel_token")
                .to_string();
            // Attaching here is what lets the open complete.
            let pipe = b.open_peer_pipe(&token).await;
            (token, pipe)
        },
        async {
            second.wait_notification("peer.channel").await["channel_token"]
                .as_str()
                .expect("channel_token")
                .to_string()
        }
    );
    let token = opened.expect("peers.channel")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string();
    let (winner_token, mut pb) = winner;
    assert_ne!(winner_token, loser_token, "one token per component");
    let mut pa = a.open_peer_pipe(&token).await;

    // The winner has the pipe.
    pa.send(b"mine").await;
    assert_eq!(pb.recv().await.as_deref(), Some(&b"mine"[..]));
    // The loser's token is spent: presenting it opens nothing.
    let mut late = b.open_peer_pipe(&loser_token).await;
    late.expect_closed().await;
    // And the pipe it lost is untouched.
    pa.send(b"still mine").await;
    assert_eq!(pb.recv().await.as_deref(), Some(&b"still mine"[..]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_channel_token_is_single_use() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (opened, _far) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": b.device_id() })),
        async {
            let offer = cb.wait_notification("peer.channel").await;
            b.open_peer_pipe(offer["channel_token"].as_str().expect("channel_token"))
                .await
        }
    );
    let token = opened.expect("peers.channel")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string();
    let mut pa = a.open_peer_pipe(&token).await;
    pa.send(b"hello").await;

    // The same token again: the grant was taken at the first attach.
    let mut twice = a.open_peer_pipe(&token).await;
    twice.expect_closed().await;
}

// ---------------------------------------------------------------------------
// Refusals: the targeting doctrine, answered to the call.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn channel_refuses_bad_targets_and_a_component_without_the_scope() {
    let server = TestServer::start().await;
    let (a, mut ca, _b, _cb) = channel_pair(&server).await;

    // Unknown device: fail-closed, indistinguishable from unattested.
    let err = ca
        .request("peers.channel", json!({ "device_id": "d_nobody" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");

    // One's own device: a loopback nothing needs.
    let err = ca
        .request("peers.channel", json!({ "device_id": a.device_id() }))
        .await
        .unwrap_err();
    assert_eq!(err.code, -32602);

    // No `device_id` at all.
    let err = ca.request("peers.channel", json!({})).await.unwrap_err();
    assert_eq!(err.code, -32602);

    // And the scope guards the door: `peers.message` does not open channels.
    let mut bare = spawn_component(&a, "bare", "custom", &["devices.read", "peers.message"]).await;
    let err = bare
        .request("peers.channel", json!({ "device_id": "d_x" }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "SCOPE_DENIED");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_attested_under_a_foreign_key_is_unknown() {
    let server = TestServer::start().await;
    // Same server account, two different trust roots: C7 must refuse.
    let (a, b) = TestCore::start_mismatched_pair(&server).await;
    let mut ca = channeller(&a, "engine", "custom").await;
    let mut cb = channeller(&b, "engine", "custom").await;
    wait_server_connected(&mut ca, true).await;
    wait_server_connected(&mut cb, true).await;
    // The record is there, relay and all; only its attestation is another
    // account's, which is the one thing that matters.
    wait_reachable(&mut ca, b.device_id()).await;

    let err = ca
        .request("peers.channel", json!({ "device_id": b.device_id() }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_with_no_route_is_offline() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    // A has a relay and is off the fake LAN; B has no relay at all, and A is not
    // on the network where B can be resolved: attested, and unreachable.
    let a = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let b = TestCore::start_lan_only_on(&server, &switchboard, &code).await;
    let mut ca = channeller(&a, "engine", "custom").await;
    let mut cb = channeller(&b, "engine", "custom").await;
    wait_server_connected(&mut ca, true).await;
    wait_server_connected(&mut cb, true).await;
    wait_attested(&mut ca, b.device_id()).await;

    let err = ca
        .request("peers.channel", json!({ "device_id": b.device_id() }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_OFFLINE");
}

#[tokio::test(flavor = "multi_thread")]
async fn component_absent_is_the_remote_cores_word() {
    let server = TestServer::start().await;
    let (_a, mut ca, b, cb) = channel_pair(&server).await;

    // The role is held remotely but WITHOUT the channel scope: absent.
    drop(cb);
    let _bare = spawn_component(&b, "bare", "custom", &["devices.read", "peers.message"]).await;
    let err = ca
        .request("peers.channel", json!({ "device_id": b.device_id() }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "COMPONENT_ABSENT");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_holder_that_never_takes_the_channel_reads_as_absent() {
    let server = TestServer::start().await;
    let (_a, mut ca, b, mut cb) = channel_pair(&server).await;

    // B's component holds the role and the scope, hears the offer, and does
    // nothing with it. `accepted` means the far end is HERE, so the caller is
    // never handed a token onto a dead pipe: it is refused, in the same words as
    // a device holding no such component at all.
    let (opened, offer) = tokio::join!(
        ca.request_within(
            "peers.channel",
            json!({ "device_id": b.device_id() }),
            Duration::from_secs(20),
        ),
        cb.wait_notification_within("peer.channel", Duration::from_secs(20)),
    );
    assert!(offer["channel_token"].is_string());
    assert_eq!(opened.unwrap_err().app_code(), "COMPONENT_ABSENT");

    // And the offer it ignored is spent: nothing opens with it later.
    let mut late = b
        .open_peer_pipe(offer["channel_token"].as_str().expect("channel_token"))
        .await;
    late.expect_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rendezvous_only_relays_refuse_the_channel_by_their_own_code() {
    let server = TestServer::start().await;
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let a = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let b = TestCore::start_enrolled_on_with_code(&server, &switchboard, Some(&code)).await;
    let mut ca = channeller(&a, "engine", "custom").await;
    let mut cb = channeller(&b, "engine", "custom").await;
    wait_server_connected(&mut ca, true).await;
    wait_server_connected(&mut cb, true).await;
    wait_reachable(&mut ca, b.device_id()).await;
    wait_reachable(&mut cb, a.device_id()).await;

    // The role in effect on the opening device (#88): the announced relays are
    // rendezvous-only above a cap. A channel names no size, so it declares
    // itself above ANY cap, and this deployment refuses it rather than quietly
    // carrying a live flow over relays that said no to exactly that.
    switchboard.set_payload_cap(&a.node_id(), Some(1024 * 1024));
    let err = ca
        .request("peers.channel", json!({ "device_id": b.device_id() }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "NO_DIRECT_PATH");
    // Nothing was offered on the far side: the refusal came before any dial
    // could reach it.
    cb.assert_silent().await;

    // A direct route appears (the shared LAN): the very same open passes. The
    // policy never blocked the channel, only a relay carrying it.
    switchboard.join_lan(&a.node_id());
    switchboard.join_lan(&b.node_id());
    let (mut pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;
    pa.send(b"direct").await;
    assert_eq!(pb.recv().await.as_deref(), Some(&b"direct"[..]));
}

// ---------------------------------------------------------------------------
// One channel per pair.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_second_open_on_the_same_pair_replaces_the_first() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (mut first_a, mut first_b) = joined(&a, &mut ca, &b, &mut cb).await;
    first_a.send(b"live").await;
    assert_eq!(first_b.recv().await.as_deref(), Some(&b"live"[..]));

    // The component opens again (after a hiccup of its own, say): the pair
    // carries ONE channel, so the newcomer takes the place instead of
    // multiplying pipes.
    //
    // B hears two things whose ORDER the network decides: the old pipe ending
    // and the new offer. Collected either way round, because asserting a
    // sequence here would be asserting on a race.
    let (opened, far) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": b.device_id() })),
        async {
            let mut closed = None;
            let mut offer = None;
            while closed.is_none() || offer.is_none() {
                let (method, params) = cb.notification().await;
                match method.as_str() {
                    "peer.channel_closed" => closed = Some(params),
                    "peer.channel" => offer = Some(params),
                    other => panic!("unexpected notification on B: {other}"),
                }
            }
            let offer = offer.expect("an offer");
            let pipe = b
                .open_peer_pipe(offer["channel_token"].as_str().expect("channel_token"))
                .await;
            (closed.expect("a closure"), pipe)
        }
    );
    let token = opened.expect("peers.channel")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string();
    let (closed_b, mut second_b) = far;
    let mut second_a = a.open_peer_pipe(&token).await;

    // Both ends of the first are told, each in the words of the Core that saw
    // the cause. The side that asked for the replacement names it (its Core
    // supersedes before it dials, so THAT one is not a race); the far side hears
    // its peer's pipe end, because a closed stream carries no reason and this
    // primitive invents none: no control frame lives inside an opaque pipe.
    assert_eq!(
        closed_reason(&mut ca, CONVERGENCE_TIMEOUT).await,
        "REPLACED"
    );
    // The far side's word is NOT asserted exactly, and that is the honest state
    // of it: its old pipe normally sees the closed stream first (`PEER_GONE`, two
    // IPC round trips ahead of anything else), but if its own Core has already
    // registered the replacement it says `REPLACED`. Both are true, and pinning
    // one of them would be pinning a race.
    let far = closed_b["reason"].as_str().expect("a reason");
    assert!(
        far == "PEER_GONE" || far == "REPLACED",
        "the far end must hear the end, in one of the two true words: {far}"
    );
    first_a.expect_closed().await;
    first_b.expect_closed().await;

    // And the second is alive.
    second_a.send(b"the new one").await;
    assert_eq!(second_b.recv().await.as_deref(), Some(&b"the new one"[..]));
}

// ---------------------------------------------------------------------------
// It dies when the trust does.
// ---------------------------------------------------------------------------

/// Two serverless Cores of one account on the same LAN, each holding the
/// other's record: the shape in which a revocation needs no server at all.
async fn serverless_pair(code: &str, switchboard: &MemorySwitchboard) -> (TestCore, TestCore) {
    let a_key = DeviceKey::generate();
    let b_key = DeviceKey::generate();
    let describe =
        |key: &DeviceKey| peer_record(key, CORE_DEVICE_NAME, std::env::consts::OS, code, 1);
    let (a_record, b_record) = (describe(&a_key), describe(&b_key));
    let a = TestCore::start_in_account_on(code, switchboard, a_key, &[b_record]).await;
    let b = TestCore::start_in_account_on(code, switchboard, b_key, &[a_record]).await;
    (a, b)
}

#[tokio::test(flavor = "multi_thread")]
async fn striking_the_peer_off_the_account_cuts_the_channel() {
    let code = onedevice_core::account_key::generate_recovery_code();
    let switchboard = MemorySwitchboard::new();
    let (a, b) = serverless_pair(&code, &switchboard).await;
    let mut ca = channeller(&a, "engine", "custom").await;
    let mut cb = channeller(&b, "engine", "custom").await;
    let mut gui = manager(&a).await;
    let (mut pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;
    pa.send(b"mid-stream").await;
    assert_eq!(pb.recv().await.as_deref(), Some(&b"mid-stream"[..]));

    // The account strikes B off, here, with no server in the picture.
    gui.request("devices.revoke", json!({ "device_id": b.device_id() }))
        .await
        .expect("devices.revoke with no server");

    // A's side names the trust that ended; B's side sees its peer's pipe end.
    assert_eq!(
        closed_reason(&mut ca, CONVERGENCE_TIMEOUT).await,
        "DEVICE_REVOKED"
    );
    assert_eq!(
        closed_reason(&mut cb, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE"
    );
    pa.expect_closed().await;
    pb.expect_closed().await;

    // And it cannot be opened again: the peer is no longer of the account.
    let err = ca
        .request("peers.channel", json!({ "device_id": b.device_id() }))
        .await
        .unwrap_err();
    assert_eq!(err.app_code(), "DEVICE_UNKNOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn logging_out_cuts_the_channel() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let mut gui = manager(&a).await;
    let (mut pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;
    pa.send(b"mid-stream").await;
    assert_eq!(pb.recv().await.as_deref(), Some(&b"mid-stream"[..]));

    gui.request("session.logout", json!({}))
        .await
        .expect("session.logout");

    assert_eq!(
        closed_reason(&mut ca, CONVERGENCE_TIMEOUT).await,
        "LOGGED_OUT"
    );
    assert_eq!(
        closed_reason(&mut cb, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE"
    );
    pa.expect_closed().await;
    pb.expect_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_channel_authorized_before_a_logout_never_starts() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let mut gui = manager(&a).await;

    // The open succeeds and the far end takes its half, but this side's
    // component has not come for its token yet: the window between an
    // authorization and a pipe.
    let (opened, _far) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": b.device_id() })),
        async {
            let offer = cb.wait_notification("peer.channel").await;
            b.open_peer_pipe(offer["channel_token"].as_str().expect("channel_token"))
                .await
        }
    );
    let token = opened.expect("peers.channel")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string();

    gui.request("session.logout", json!({}))
        .await
        .expect("session.logout");

    // The component comes for a channel the session no longer allows: the pipe
    // never starts, and it is told the cause rather than left to guess from a
    // connection that simply closed.
    let mut late = a.open_peer_pipe(&token).await;
    assert_eq!(
        closed_reason(&mut ca, CONVERGENCE_TIMEOUT).await,
        "LOGGED_OUT"
    );
    late.expect_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_local_end_closing_ends_the_channel_at_both_ends() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;

    // The component drops its pipe (what a crashed consumer looks like): its own
    // Core words it as a local close, the far end as its peer going.
    drop(pa);
    assert_eq!(closed_reason(&mut ca, CONVERGENCE_TIMEOUT).await, "CLOSED");
    assert_eq!(
        closed_reason(&mut cb, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE"
    );
    pb.expect_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_core_stopping_ends_the_channel_at_the_far_end() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (mut pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;
    pa.send(b"before the stop").await;
    assert_eq!(pb.recv().await.as_deref(), Some(&b"before the stop"[..]));

    // A's Core stops. Its pipe is NOT one of the connections the teardown
    // sweeps (a second connection is never registered as one), so the pipe has
    // to be ended on purpose: the mass cut does it immediately, and each vigil's
    // owner check would within a poll anyway.
    drop(a);

    // The word reaches this side's component, and that ORDER is the whole point:
    // the teardown closes that very connection, and a connection's queue is
    // drained only up to its close, so a reason queued afterwards would never be
    // written at all.
    assert_eq!(
        closed_reason(&mut ca, CONVERGENCE_TIMEOUT).await,
        "SHUTDOWN"
    );
    // And the far end hears the end rather than waiting on a mute pipe.
    assert_eq!(
        closed_reason(&mut cb, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE"
    );
    pa.expect_closed().await;
    pb.expect_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_component_dying_whole_ends_the_channel_at_the_far_end() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;

    // The whole component goes: its control connection AND its pipe. Nobody is
    // left to tell on this side, which is exactly why the far end must be told
    // rather than left holding a mute pipe.
    drop(pa);
    drop(ca);
    assert_eq!(
        closed_reason(&mut cb, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE"
    );
    pb.expect_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_component_that_leaves_the_control_plane_loses_its_pipe() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (mut pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;

    // Only the CONTROL connection goes; the pipe connection is left wide open,
    // and both peers are perfectly present. The grant was bound to that
    // connection, so the channel does not outlive it: nothing else here would
    // have noticed.
    drop(ca);
    assert_eq!(
        closed_reason(&mut cb, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE"
    );
    pa.expect_closed().await;
    pb.expect_closed().await;
}

// ---------------------------------------------------------------------------
// The caps.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_frame_above_the_cap_from_the_component_ends_the_channel() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (mut pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;

    // Announced, not sent: the length alone is refused, before one byte of it
    // is allocated.
    pa.send_announced_len(1025).await;
    assert_eq!(
        closed_reason(&mut ca, CONVERGENCE_TIMEOUT).await,
        "FRAME_TOO_LARGE"
    );
    assert_eq!(
        closed_reason(&mut cb, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE"
    );
    pb.expect_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_burst_is_forgiven_and_a_sustained_flood_is_cut() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (mut pa, pb) = joined(&a, &mut ca, &b, &mut cb).await;
    // The far end reads continuously: what is under test is the rate the Core
    // reads at, not a component that stopped reading (which is backpressure, and
    // a different rule).
    let drain = tokio::spawn(async move {
        let mut pb = pb;
        while pb.recv().await.is_some() {}
    });

    // Far above the 4000 frames per second the cap allows (itself four times the
    // highest rate this primitive was designed for, a 1000 Hz mouse), in ONE
    // window. Empty frames written in one go: the test must not be the
    // bottleneck that keeps the flow under the cap.
    //
    // Forgiven, deliberately: this is the shape a legitimate flow takes when a
    // network stall backs it up and the backlog then drains at socket speed.
    // Cutting here would blame the component for a hiccup it did not cause.
    pa.send_burst(5000, &[]).await;
    ca.assert_silent().await;

    // A second window over the cap is not a hiccup any more.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    pa.send_burst(5000, &[]).await;
    assert_eq!(
        closed_reason(&mut ca, CONVERGENCE_TIMEOUT).await,
        "RATE_EXCEEDED"
    );
    drain.await.expect("the draining end");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_channel_silent_in_both_directions_is_swept() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (mut pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;

    // Nothing is sent by either end. The idle budget is the CHANNEL's, so a
    // component that wants to keep one open has to say something; one frame
    // from either side would have been enough, an empty one included.
    //
    // Each Core runs its own budget on its own clock, and they expire within
    // milliseconds of each other: whichever sweeps first cuts, and the other end
    // then hears its peer's pipe end. So the assertion is that BOTH ends are
    // told and that the sweep is what happened, not which of two near-identical
    // clocks won.
    let reasons = [
        closed_reason(&mut ca, Duration::from_secs(25)).await,
        closed_reason(&mut cb, Duration::from_secs(25)).await,
    ];
    assert!(
        reasons.contains(&"IDLE_TIMEOUT".to_string()),
        "the sweep must be named by the end that swept: {reasons:?}"
    );
    assert!(
        reasons
            .iter()
            .all(|r| r == "IDLE_TIMEOUT" || r == "PEER_GONE"),
        "both ends end, and only those two words fit: {reasons:?}"
    );
    pa.expect_closed().await;
    pb.expect_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn traffic_in_one_direction_keeps_the_channel_past_the_idle_budget() {
    let server = TestServer::start().await;
    let (a, mut ca, b, mut cb) = channel_pair(&server).await;
    let (mut pa, mut pb) = joined(&a, &mut ca, &b, &mut cb).await;

    // One direction only, at a cadence a component would pick for a keepalive:
    // the channel outlives the idle budget, because the budget is the channel's
    // and not each direction's.
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        pb.send(&[]).await;
    }
    pa.send(b"still here").await;
    assert_eq!(pb.recv().await.as_deref(), Some(&b"still here"[..]));
    ca.assert_silent().await;
    cb.assert_silent().await;
}

// ---------------------------------------------------------------------------
// The wire, spoken by hand: the shape frozen, and the far end misbehaving.
// ---------------------------------------------------------------------------

/// A Core with a channeller, plus a raw attested peer of the same account that
/// the Core already sees (so its streams pass the directory check).
async fn core_with_raw_peer(server: &TestServer) -> (TestCore, TestComponent, RawPeer) {
    let switchboard = MemorySwitchboard::new();
    let code = onedevice_core::account_key::generate_recovery_code();
    let a = TestCore::start_enrolled_on_with_code(server, &switchboard, Some(&code)).await;
    let mut ca = channeller(&a, "engine", "custom").await;
    wait_server_connected(&mut ca, true).await;
    let raw = RawPeer::attested(server, &switchboard, &code).await;
    wait_attested(&mut ca, &raw.device_id).await;
    (a, ca, raw)
}

/// Answers the next `peer_channel` opening by hand, asserting the frame's shape,
/// and hands the stream back: from there the far end is the test's to misbehave
/// with.
async fn accept_by_hand(raw: &RawPeer, role: &str) -> Box<dyn onedevice_core::IoStream> {
    let (frame, mut stream) = raw
        .next_open_of("peer_channel")
        .await
        .expect("a peer_channel opening");
    assert_eq!(
        frame["role"],
        json!(role),
        "the role is stamped by the opener's Core"
    );
    peer_write(
        &mut stream,
        &json!({ "type": "peer_channel_ack", "accepted": true }),
    )
    .await;
    stream
}

/// Opens a data-plane stream from the raw peer to `target`.
async fn dial(raw: &RawPeer, target: &TestCore) -> Box<dyn onedevice_core::IoStream> {
    let peer = onedevice_core::PeerAddr {
        node_id: target.key().node_id(),
        relay_url: Some(format!("iroh+memory://{}", target.key().node_id())),
        addrs: Vec::new(),
    };
    raw.transport
        .open(&peer)
        .await
        .expect("the switchboard routes")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_wire_shape_is_the_role_out_and_an_acceptance_back() {
    let server = TestServer::start().await;
    let (a, mut ca, raw) = core_with_raw_peer(&server).await;

    let (opened, mut far) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": raw.device_id })),
        accept_by_hand(&raw, "custom"),
    );
    let token = opened.expect("peers.channel")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string();
    let mut pa = a.open_peer_pipe(&token).await;

    // From the ack on, that stream IS the pipe: the same framing, opaque both
    // ways, with no envelope of the Core's around it.
    pa.send(b"to the peer").await;
    let mut len = [0u8; 4];
    far.read_exact(&mut len).await.expect("a frame length");
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    far.read_exact(&mut body).await.expect("a frame body");
    assert_eq!(body, b"to the peer");

    far.write_all(&4u32.to_be_bytes()).await.expect("length");
    far.write_all(b"back").await.expect("body");
    far.flush().await.expect("flush");
    assert_eq!(pa.recv().await.as_deref(), Some(&b"back"[..]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_refuses_is_component_absent_and_one_that_answers_otherwise_is_offline() {
    let server = TestServer::start().await;
    let (_a, mut ca, raw) = core_with_raw_peer(&server).await;

    // The far Core says nobody there holds the role with the scope.
    let (opened, ()) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": raw.device_id })),
        async {
            let (_frame, mut stream) = raw
                .next_open_of("peer_channel")
                .await
                .expect("a peer_channel opening");
            peer_write(
                &mut stream,
                &json!({ "type": "peer_channel_ack", "accepted": false }),
            )
            .await;
        }
    );
    assert_eq!(opened.unwrap_err().app_code(), "COMPONENT_ABSENT");

    // Anything that is not that ack (a frame from another era, a tombstone)
    // reads as the peer being unreachable for this purpose.
    let (opened, ()) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": raw.device_id })),
        async {
            let (_frame, mut stream) = raw
                .next_open_of("peer_channel")
                .await
                .expect("a peer_channel opening");
            peer_write(&mut stream, &json!({ "type": "clip_ack" })).await;
        }
    );
    assert_eq!(opened.unwrap_err().app_code(), "DEVICE_OFFLINE");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_frame_above_the_cap_from_the_peer_ends_the_channel() {
    let server = TestServer::start().await;
    let (a, mut ca, raw) = core_with_raw_peer(&server).await;
    let (opened, mut far) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": raw.device_id })),
        accept_by_hand(&raw, "custom"),
    );
    let token = opened.expect("peers.channel")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string();
    let mut pa = a.open_peer_pipe(&token).await;

    // A compliant Core caps at 1 KiB; a compromised device of the account can
    // announce anything. The cap is enforced on arrival too, and the component
    // is told which cap it was.
    far.write_all(&(64u32 * 1024).to_be_bytes())
        .await
        .expect("an oversized length");
    far.flush().await.expect("flush");
    assert_eq!(
        closed_reason(&mut ca, CONVERGENCE_TIMEOUT).await,
        "FRAME_TOO_LARGE"
    );
    pa.expect_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_vanishing_mid_stream_ends_the_channel() {
    let server = TestServer::start().await;
    let (a, mut ca, raw) = core_with_raw_peer(&server).await;
    let (opened, far) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": raw.device_id })),
        accept_by_hand(&raw, "custom"),
    );
    let token = opened.expect("peers.channel")["channel_token"]
        .as_str()
        .expect("channel_token")
        .to_string();
    let mut pa = a.open_peer_pipe(&token).await;
    pa.send(b"anyone there").await;

    // The far end simply goes: no terminal frame, no negotiation.
    drop(far);
    assert_eq!(
        closed_reason(&mut ca, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE"
    );
    pa.expect_closed().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_or_unheld_opening_is_refused_and_reaches_no_component() {
    let server = TestServer::start().await;
    let (a, mut ca, raw) = core_with_raw_peer(&server).await;

    // A `peer_channel` with no role at all: refused.
    let mut stream = dial(&raw, &a).await;
    peer_write(&mut stream, &json!({ "type": "peer_channel" })).await;
    let ack = peer_read(&mut stream).await.expect("an ack");
    assert_eq!(ack["type"], json!("peer_channel_ack"));
    assert_eq!(ack["accepted"], json!(false));

    // A role nobody holds here: the same answer, deliberately (the ack owes the
    // sender no inventory of this device's components).
    let mut stream = dial(&raw, &a).await;
    peer_write(
        &mut stream,
        &json!({ "type": "peer_channel", "role": "menu-backend" }),
    )
    .await;
    let ack = peer_read(&mut stream).await.expect("an ack");
    assert_eq!(ack["accepted"], json!(false));

    // And the component of the sender's own role heard nothing of either.
    ca.assert_silent().await;
}
