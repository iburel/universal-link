// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! `PeerChannel` (#126) against TWO real Cores of one account, in one process:
//! the client half of `peers.channel` (#124), consumed exactly as the input
//! component will consume it.
//!
//! What these tests pin is the seam between the two planes, because that seam is
//! the one thing a caller can get wrong for free: the bytes and their order are
//! the pipe's business, while the REASON a channel ended is only ever readable on
//! the control connection, as `peer.channel_closed`. So every end here is
//! asserted twice, once as the pipe going mute and once as the word for it
//! arriving on the control plane.
//!
//! Two harness notes worth knowing before adding a test:
//! - `peers.channel` does not return until the far end has ATTACHED, so a test
//!   that calls it and only then goes looking for the `peer.channel` offer
//!   deadlocks on itself. The two halves are raced with `tokio::join!`.
//! - the call's budget is the Core's whole handshake, up to 25 s
//!   (`PEER_CHANNEL_REQUEST_TIMEOUT`), far above this suite's default
//!   `request_timeout`. A channeller here is configured with it, for the very
//!   reason a real component must be: otherwise the client's own timeout is what
//!   answers, and an honest refusal reads as a silence.

use std::time::Duration;

use onedevice_ipc_client::{
    Client, MAX_PEER_FRAME, PEER_CHANNEL_REQUEST_TIMEOUT, PeerChannel, PeerSendError, RequestError,
};
use serde_json::json;
use tokio::time::timeout;

use crate::support::*;

/// What a component that opens and receives peer channels holds. NO topic is
/// subscribed anywhere in this module (`component` subscribes none): that is how
/// the suite proves the `peers.channel` scope is itself the subscription for
/// `peer.channel` and `peer.channel_closed`.
const CHANNEL_SCOPES: &[&str] = &["peers.channel", "devices.read", "session.read"];

/// A GUI-shaped component: what a test needs in order to log out.
const MANAGER_SCOPES: &[&str] = &["session.manage", "session.read"];

/// Two Cores of one account, each with a `custom`-role channeller that already
/// sees the other as reachable. `custom` because it is the role a third-party
/// engine gets, and one the Core does not treat as exclusive.
struct Pair {
    a: TestCore,
    ca: Client,
    ea: Events,
    b: TestCore,
    /// The far end's client. HELD and never spoken to: it is what makes B a
    /// device where the role and the scope are present, and no channel outlives
    /// it (the grant is bound to that control connection, so dropping it is a
    /// `PEER_GONE`). Underscored because holding it is its whole job.
    _cb: Client,
    eb: Events,
}

impl Pair {
    async fn start(server: &TestServer) -> Pair {
        let (a, b) = TestCore::start_pair(server).await;
        let (ca, ea) = component(
            &a,
            "engine",
            "custom",
            CHANNEL_SCOPES,
            PEER_CHANNEL_REQUEST_TIMEOUT,
        )
        .await;
        let (cb, eb) = component(
            &b,
            "engine",
            "custom",
            CHANNEL_SCOPES,
            PEER_CHANNEL_REQUEST_TIMEOUT,
        )
        .await;
        wait_server_connected(&ca, true).await;
        wait_server_connected(&cb, true).await;
        wait_reachable(&ca, b.device_id()).await;
        wait_reachable(&cb, a.device_id()).await;
        Pair {
            a,
            ca,
            ea,
            b,
            _cb: cb,
            eb,
        }
    }

    /// Opens a channel from A to B and takes it at both ends: two `PeerChannel`s
    /// from the two ways a token arrives (a reply here, a notification there).
    ///
    /// The halves are raced on purpose, see this module's header.
    async fn joined(&mut self) -> (PeerChannel, PeerChannel) {
        // Field by field: one future needs the client, the other needs the far
        // end's events mutably, and they run at the same time.
        let Pair { a, ca, b, eb, .. } = self;
        let (a_id, b_id) = (a.device_id().to_string(), b.device_id().to_string());
        let (ipc_a, ipc_b) = (a.ipc_path(), b.ipc_path());
        let (opened, far) = tokio::join!(
            ca.request("peers.channel", json!({ "device_id": b_id })),
            async {
                let offer = eb.wait_notification("peer.channel").await;
                assert_eq!(
                    offer["device_id"],
                    json!(a_id),
                    "the offer names the device that opened the channel"
                );
                let token = offer["channel_token"]
                    .as_str()
                    .expect("a channel_token on the offer");
                PeerChannel::open(&ipc_b, token)
                    .await
                    .expect("the far end attaches to its half")
            }
        );
        let token = opened.expect("peers.channel")["channel_token"]
            .as_str()
            .expect("a channel_token in the reply")
            .to_string();
        let near = PeerChannel::open(&ipc_a, &token)
            .await
            .expect("the near end attaches to its half");
        (near, far)
    }
}

/// The next frame, which must BE a frame. `None` would mean the pipe ended, and a
/// test that expected bytes says exactly that rather than turning a `None` into
/// an unrelated panic.
async fn next_frame(pipe: &mut PeerChannel) -> Vec<u8> {
    match timeout(RESPONSE_TIMEOUT, pipe.recv()).await {
        Ok(Some(Ok(bytes))) => bytes,
        Ok(Some(Err(e))) => panic!("the peer channel failed instead of carrying a frame: {e}"),
        Ok(None) => panic!("the peer channel ended instead of carrying a frame"),
        Err(_) => panic!("timeout waiting for a frame on the peer channel"),
    }
}

/// Asserts the pipe ENDED. What ended it is deliberately not asserted here: that
/// word lives on the control plane, and every caller of this checks it there.
async fn pipe_ended(pipe: &mut PeerChannel) {
    match timeout(RESPONSE_TIMEOUT, pipe.recv()).await {
        Ok(None) => {}
        Ok(other) => panic!("the peer channel is still open: {other:?}"),
        Err(_) => panic!("timeout waiting for the peer channel to end"),
    }
}

/// Asserts nothing arrives on the pipe during `SILENCE_WINDOW` and that it stays
/// OPEN. Dropping the `recv` future at the end of the window loses no frame: the
/// read half is owned by a task feeding a channel, which is what makes `recv`
/// cancel-safe in the first place.
async fn pipe_silent(pipe: &mut PeerChannel) {
    match timeout(SILENCE_WINDOW, pipe.recv()).await {
        Err(_) => {}
        Ok(None) => panic!("the peer channel ended during the silence window"),
        Ok(Some(frame)) => panic!("unexpected frame on the peer channel: {frame:?}"),
    }
}

/// The `reason` of the one `peer.channel_closed` the local end is owed. The
/// budget is the caller's: most reasons are immediate, some are a Core-side
/// budget elapsing.
async fn closed_reason(events: &mut Events, budget: Duration) -> String {
    let closed = events
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
async fn a_peer_channel_carries_opaque_bytes_verbatim_in_both_directions() {
    let server = TestServer::start().await;
    let mut pair = Pair::start(&server).await;
    let (mut near, mut far) = pair.joined().await;

    // Arbitrary bytes, not JSON, and not a byte changed: the Core relays them and
    // interprets none of them.
    near.send(&[0x00, 0x01, 0xff, 0x7f])
        .await
        .expect("send down");
    assert_eq!(
        next_frame(&mut far).await,
        vec![0x00, 0x01, 0xff, 0x7f],
        "the far end reads the bytes the near end wrote, verbatim"
    );
    far.send(b"up").await.expect("send up");
    assert_eq!(
        next_frame(&mut near).await,
        b"up".to_vec(),
        "the pipe is duplex: neither end is the driver"
    );

    // A zero-length frame is legal, and is what a keepalive looks like to a Core
    // that does not know what a keepalive is.
    far.send(&[]).await.expect("send a keepalive");
    assert_eq!(
        next_frame(&mut near).await,
        Vec::<u8>::new(),
        "a zero-length frame is carried, not swallowed"
    );

    // Exactly AT the cap: carried. One byte above it never leaves (below).
    let full = vec![0x5a; MAX_PEER_FRAME];
    near.send(&full).await.expect("send a frame at the cap");
    assert_eq!(
        next_frame(&mut far).await,
        full,
        "a frame of exactly MAX_PEER_FRAME bytes is legal"
    );

    // Order is preserved, which is the whole point for a flow of numbered
    // positions: frame 3 after frame 2, or the flow is worthless.
    for n in 0u8..16 {
        near.send(&[n]).await.expect("send in order");
    }
    for n in 0u8..16 {
        assert_eq!(
            next_frame(&mut far).await,
            vec![n],
            "frames arrive in the order they were sent"
        );
    }

    // And nothing was said on either control plane, because nothing ended.
    pair.ea.assert_no_notification("peer.channel_closed").await;
    pair.eb.assert_no_notification("peer.channel_closed").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_far_end_is_offered_the_channel_by_its_scope_with_no_subscription() {
    let server = TestServer::start().await;
    let mut pair = Pair::start(&server).await;
    let Pair {
        a, ca, ea, b, eb, ..
    } = &mut pair;
    let (a_id, b_id) = (a.device_id().to_string(), b.device_id().to_string());
    let ipc_b = b.ipc_path();

    // The far component subscribed to NO topic (`component` passes none): the
    // `peers.channel` scope is its subscription, and this offer arriving is the
    // proof. It carries its OWN token, which is what it opens its half with.
    let (opened, far) = tokio::join!(
        ca.request("peers.channel", json!({ "device_id": b_id })),
        async {
            let offer = eb.wait_notification("peer.channel").await;
            assert_eq!(
                offer["device_id"],
                json!(a_id),
                "the offer names the opener"
            );
            let token = offer["channel_token"]
                .as_str()
                .expect("a channel_token on the offer")
                .to_string();
            let pipe = PeerChannel::open(&ipc_b, &token)
                .await
                .expect("the offered token opens a pipe");
            (token, pipe)
        }
    );
    let (offered_token, mut far) = far;
    let near_token = opened.expect("peers.channel")["channel_token"]
        .as_str()
        .expect("a channel_token in the reply")
        .to_string();
    assert_ne!(
        offered_token, near_token,
        "each end presents its own token: one minted for the opener, one for the far component"
    );
    let mut near = PeerChannel::open(&a.ipc_path(), &near_token)
        .await
        .expect("the reply's token opens the near half");

    // The pipe that notification bought is a working one, in the direction the
    // far end never asked for.
    far.send(b"offered and taken").await.expect("send up");
    assert_eq!(
        next_frame(&mut near).await,
        b"offered and taken".to_vec(),
        "the channel a notification announced carries bytes like any other"
    );
    // The opener is told by its REPLY, never by a notification: no `peer.channel`
    // is pushed to the side that asked for one.
    ea.assert_no_notification("peer.channel").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_frame_above_the_cap_is_refused_locally_and_the_channel_survives() {
    let server = TestServer::start().await;
    let mut pair = Pair::start(&server).await;
    let (mut near, mut far) = pair.joined().await;

    // One byte above the cap. The refusal is LOCAL and named: had the frame been
    // written, the Core would have cut the whole channel with FRAME_TOO_LARGE and
    // a caller asking for one oversized message would have lost its pipe.
    let oversized = vec![0x11; MAX_PEER_FRAME + 1];
    match near.send(&oversized).await {
        Err(PeerSendError::TooLarge { len }) => assert_eq!(
            len,
            MAX_PEER_FRAME + 1,
            "the refusal names the length that was refused"
        ),
        other => panic!("expected a local TooLarge refusal, got {other:?}"),
    }

    // Nothing was written, so the far end sees nothing at all: the guard sits
    // before the socket, it is not an error read back from it.
    pipe_silent(&mut far).await;

    // The whole point of guarding locally: the channel is untouched and still
    // usable, in both directions, right up to the cap.
    let legal = vec![0x22; MAX_PEER_FRAME];
    near.send(&legal)
        .await
        .expect("the channel survived a locally refused frame");
    assert_eq!(
        next_frame(&mut far).await,
        legal,
        "a legal frame goes through after a refused one"
    );
    far.send(b"still here").await.expect("send up");
    assert_eq!(next_frame(&mut near).await, b"still here".to_vec());

    // And no end was worded on either control plane, because none happened.
    pair.ea.assert_no_notification("peer.channel_closed").await;
    pair.eb.assert_no_notification("peer.channel_closed").await;
}

// ---------------------------------------------------------------------------
// The ends, and the word for them on the control plane.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_local_end_ends_the_far_end_and_each_side_hears_its_own_word() {
    let server = TestServer::start().await;
    let mut pair = Pair::start(&server).await;
    let (near, mut far) = pair.joined().await;

    // `Drop` tears the connection down at once (the reader task is aborted, both
    // halves close), which is also what a crashed consumer looks like. The far end
    // must not be left holding a mute pipe until a Core-side budget elapses.
    drop(near);

    assert_eq!(
        closed_reason(&mut pair.ea, CONVERGENCE_TIMEOUT).await,
        "CLOSED",
        "the side that closed is told that it closed"
    );
    assert_eq!(
        closed_reason(&mut pair.eb, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE",
        "the far side is told its peer went, which is all a closed stream can say"
    );
    pipe_ended(&mut far).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn logging_out_ends_the_channel_and_the_reason_is_read_off_the_control_plane() {
    let server = TestServer::start().await;
    let mut pair = Pair::start(&server).await;
    let (gui, _gui_events) =
        component(&pair.a, "gui", "gui", MANAGER_SCOPES, RESPONSE_TIMEOUT).await;
    let (mut near, mut far) = pair.joined().await;
    near.send(b"mid-stream").await.expect("send mid-stream");
    assert_eq!(next_frame(&mut far).await, b"mid-stream".to_vec());

    // A real teardown from the Core's own vocabulary, staged the cheapest honest
    // way there is: the account's grants do not outlive its session.
    gui.request("session.logout", json!({}))
        .await
        .expect("session.logout");

    // Both pipes go mute, and neither carries a syllable about why: the reason is
    // a control-plane notification or it is nothing.
    pipe_ended(&mut near).await;
    pipe_ended(&mut far).await;
    assert_eq!(
        closed_reason(&mut pair.ea, CONVERGENCE_TIMEOUT).await,
        "LOGGED_OUT",
        "the side that saw the cause names it"
    );
    assert_eq!(
        closed_reason(&mut pair.eb, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE",
        "the far side hears only that its peer went"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_core_stopping_is_surfaced_as_peer_gone() {
    let server = TestServer::start().await;
    let mut pair = Pair::start(&server).await;
    let (mut near, _far) = pair.joined().await;
    near.send(b"before the stop").await.expect("send");

    // The whole far machine goes: its Core, and with it the peer stream. The
    // second most common real end after a close, and the one nothing local would
    // otherwise reveal. (Moved out of the pair on purpose: what has to happen here
    // is the pair losing half of itself.)
    drop(pair.b);

    pipe_ended(&mut near).await;
    assert_eq!(
        closed_reason(&mut pair.ea, CONVERGENCE_TIMEOUT).await,
        "PEER_GONE",
        "a peer whose Core stopped is worded PEER_GONE, not left as a mute pipe"
    );
}

// ---------------------------------------------------------------------------
// The budget: the client's own timeout must not be what answers.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_peers_channel_call_is_given_the_cores_whole_handshake_budget() {
    let server = TestServer::start().await;
    let mut pair = Pair::start(&server).await;
    let b_id = pair.b.device_id().to_string();

    // Staged on the Core's slowest HONEST answer: the far component holds the
    // role and the scope, is offered the channel, and never comes to take it. The
    // far Core waits out its attach grace (5 s) before answering
    // COMPONENT_ABSENT, so that answer cannot arrive within the suite's default
    // request budget - if the client's timeout were what decided, this would be a
    // `Timeout` and the caller could not tell a refusal from a silence.
    let started = std::time::Instant::now();
    let err = pair
        .ca
        .request("peers.channel", json!({ "device_id": b_id }))
        .await
        .expect_err("a holder that never attaches is a refusal");
    let elapsed = started.elapsed();
    assert_eq!(
        app_code(&err),
        "COMPONENT_ABSENT",
        "the Core's own word for a holder that never came, not the client's Timeout"
    );
    assert!(
        elapsed > RESPONSE_TIMEOUT,
        "this test only proves anything if the answer comes after the suite's default \
         request budget ({RESPONSE_TIMEOUT:?}); it took {elapsed:?}"
    );
    // The offer really was delivered: the far end simply never attached.
    assert!(
        pair.eb.wait_notification("peer.channel").await["channel_token"].is_string(),
        "the far end was offered the channel it never took"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tighter_request_budget_turns_the_cores_refusal_into_a_bare_timeout() {
    let server = TestServer::start().await;
    let pair = Pair::start(&server).await;
    let b_id = pair.b.device_id().to_string();

    // What PEER_CHANNEL_REQUEST_TIMEOUT exists to prevent, and the reason it is
    // public API rather than a number inside a component: a caller that budgets
    // the call by its own impatience gets `Timeout` for an answer the Core was on
    // its way to giving. The regression is in honesty, not in speed.
    let (impatient, _events) = component(
        &pair.a,
        "impatient",
        "custom",
        CHANNEL_SCOPES,
        Duration::from_secs(1),
    )
    .await;
    let err = impatient
        .request("peers.channel", json!({ "device_id": b_id }))
        .await
        .expect_err("a holder that never attaches is a refusal");
    assert!(
        matches!(err, RequestError::Timeout),
        "expected the client's own Timeout under a 1 s budget, got {err:?}"
    );
}
