// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The directory's network plane: two devices of the account tell each other
//! whom they know. This is what makes an account without a server more than a
//! collection of devices that happen to share a key — B learns of C from A, and a
//! device struck off on one machine is struck off on the others.
//!
//! # The protocol (over `dataplane::ALPN`, one bidirectional stream)
//!
//! ```text
//! initiator → responder   { "type": "dir_sync",   "records": [...], "revoked": {...} }
//! responder → initiator   { "type": "dir_roster", "records": [...], "revoked": {...} }
//! ```
//!
//! One round trip, both frames the same shape ([`Roster`]), both sides absorbing
//! what they received. No new ALPN: the data plane dispatches on the first
//! frame's `type` (`offer`, `clip_*`, and now this), so a peer that speaks
//! `ul/data/1` already has a stream to say it on.
//!
//! The responder answers with the roster it held BEFORE absorbing. Replying
//! afterwards would echo back what the initiator has just told it — a second
//! copy of everything, for nothing.
//!
//! # What the exchange is worth
//!
//! Nothing is trusted because a peer said it. Every record must be signed by the
//! device it describes and attested under our account key, every tombstone must
//! be signed by that account key ([`crate::directory::shareable`],
//! [`crate::state::SessionState::absorb`]) — so relaying is a courier's job, not a
//! witness's, and a compromised member of the account cannot invent a sibling,
//! rename one, or bring a struck-off one back. What it CAN do is stay silent, and
//! silence costs nothing: the account is not made of what a peer chooses to
//! mention.
//!
//! Both ends must already know each other for the stream to exist at all
//! (`dataplane::peer_in_directory` on the way in, `account_peers` on the way out).
//! That is the point where gossip needs a first introduction, and giving one is
//! the pairing brick's job — not this one's.
//!
//! # When it runs
//!
//! On a change of LAN membership (a sibling appears: the moment it becomes
//! reachable is the moment to talk to it), on an explicit nudge
//! (`AppState::dirsync_wake` — a local rename or revocation, which peers should
//! not wait a quarter of an hour to hear), and on a slow tick as a safety net.
//! Every peer with a route is synced with in the round, so on a LAN the account
//! converges in one.
//!
//! # And where there IS a server
//!
//! The task runs the same, and gates on nothing — but what it can carry there is
//! almost nothing, and that is the honest outcome rather than a special case: a
//! record a server minted carries no self-signature, so it is not
//! [`shareable`](crate::directory::shareable) and never leaves this device. What
//! does travel is the tombstones, which are the account's own signatures and owe
//! the server nothing. So a deployment keeps its authority over names by
//! construction, and a device struck off with no server in sight stays struck off
//! on a sibling that has one.
//!
//! The consequence is the one to keep in view: a device enrolled on a server is
//! invisible to this exchange, so the account cannot yet be half on a server and
//! half not. Making the two halves one account is the continuum brick's job.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::connector::IoStream;
use crate::dataplane::{self, PeerAddr};
use crate::directory::Roster;
use crate::state::{AppState, enrich_device};

/// Frame type: "here is what I know, tell me what you know".
const SYNC: &str = "dir_sync";
/// Frame type: the answer, the same shape.
const ROSTER: &str = "dir_roster";

/// Safety net between two rounds. Long on purpose: the LAN watcher and the
/// explicit nudge do the timely work, and this only catches what those two
/// missed (a peer reachable by relay the whole time, a wake lost to a crash).
const SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Budget for establishing the stream — a relay handshake can take seconds.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Budget for the exchange itself. A roster is a few kilobytes: a peer that
/// takes longer than this is not answering.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
/// What the responder allows itself, after its reply, to see the initiator close.
/// Dropping the stream any earlier would abandon the reply in flight on the QUIC
/// side — the same lifecycle the transfer protocol learned the hard way.
const LINGER: Duration = Duration::from_secs(5);

/// The sync task, alive for the life of the Core. Runs a round, then waits for
/// the next reason to run one.
pub(crate) async fn run(state: Arc<AppState>) {
    let mut changes = state.transport.lan_changes();
    // A transport without LAN discovery hands out a receiver whose sender is
    // already gone. Its arm is disabled at the first error rather than left to
    // return `Err` forever, which would spin this loop.
    let mut watching_lan = true;
    loop {
        sync_round(&state).await;
        tokio::select! {
            outcome = changes.changed(), if watching_lan => {
                if outcome.is_err() {
                    watching_lan = false;
                }
            }
            _ = state.dirsync_wake.notified() => {}
            _ = tokio::time::sleep(SYNC_INTERVAL) => {}
        }
    }
}

/// One round: exchange rosters with every device of the account we have a route
/// to. Sequential — a roster is tiny, the peers are few, and each exchange is
/// bounded, so nothing here can outlast a peer that goes quiet. Each exchange
/// uses what we know AT THAT MOMENT, so what the first peer teaches us reaches
/// the second within the same round.
async fn sync_round(state: &Arc<AppState>) {
    for peer in dataplane::account_peers(state) {
        if let Err(e) = sync_with(state, &peer).await {
            // Debug: a peer that is asleep, or off the network between the
            // address-book snapshot and the dial, is the ordinary case.
            tracing::debug!(peer = %peer.node_id, error = %e, "directory exchange failed");
        }
    }
}

/// The initiator half: offer our roster, take in theirs.
async fn sync_with(state: &Arc<AppState>, peer: &PeerAddr) -> std::io::Result<()> {
    // Nothing to say (no trust root): we do not even open the stream.
    let Some(ours) = our_roster(state).filter(|roster| !roster.is_empty()) else {
        return Ok(());
    };
    let mut stream = timed(CONNECT_TIMEOUT, state.transport.open(peer), "connect").await?;
    write_roster(&mut stream, SYNC, &ours).await?;
    let reply = timed(
        EXCHANGE_TIMEOUT,
        dataplane::read_frame(&mut stream),
        "their roster",
    )
    .await?;
    let reply: Value = serde_json::from_slice(&reply)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if reply.get("type").and_then(Value::as_str) != Some(ROSTER) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the answer is not a roster",
        ));
    }
    let theirs = Roster::parse(&reply)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "unreadable roster"))?;
    absorb(state, &theirs);
    let _ = stream.shutdown().await;
    Ok(())
}

/// The responder half, reached from the data plane's frame dispatch: answer with
/// what we hold, then take in what they sent.
pub(crate) async fn recv_sync(
    state: Arc<AppState>,
    peer: String,
    first: Value,
    mut stream: Box<dyn IoStream>,
) {
    // Read out BEFORE absorbing, so the answer does not echo their own roster
    // back at them.
    let Some(ours) = our_roster(&state) else {
        tracing::debug!(peer = %peer, "a peer offers a roster and we hold no trust root: ignored");
        return;
    };
    let Some(theirs) = Roster::parse(&first) else {
        tracing::warn!(peer = %peer, "unreadable roster from a peer: ignored");
        return;
    };
    if let Err(e) = write_roster(&mut stream, ROSTER, &ours).await {
        tracing::debug!(peer = %peer, error = %e, "failed to answer a roster");
        return;
    }
    absorb(&state, &theirs);
    // The initiator closes once it has read the answer: holding the stream until
    // then is what proves the answer arrived.
    let _ = stream.shutdown().await;
    let _ = tokio::time::timeout(LINGER, dataplane::drain(&mut stream)).await;
}

async fn write_roster(
    stream: &mut Box<dyn IoStream>,
    kind: &str,
    roster: &Roster,
) -> std::io::Result<()> {
    let mut frame = roster.payload();
    frame["type"] = json!(kind);
    let bytes = serde_json::to_vec(&frame).expect("a roster serializes");
    timed(
        EXCHANGE_TIMEOUT,
        dataplane::write_frame(stream, &bytes),
        "our roster",
    )
    .await
}

/// What we have to offer. `None` without a trust root: we cannot tell a member
/// from a stranger, so we have nothing to say about the account — fail-closed,
/// like every other door the account key guards.
///
/// Shared with the pairing (`pairing::lan_tail`), which ends by handing the other
/// device exactly this: the two halves of an introduction are a roster each.
pub(crate) fn our_roster(state: &AppState) -> Option<Roster> {
    // Leaf lock released before taking `session` (lock ordering).
    let ak_pub = state
        .account_root
        .lock()
        .expect("lock account_root")
        .as_ref()
        .map(|root| root.ak_pub.clone())?;
    let s = state.session.lock().expect("lock session");
    Some(Roster::of(s.devices.as_ref(), &s.revoked, &ak_pub))
}

/// Takes in a peer's roster and tells the IPC what changed. Silent — not one
/// notification, not one disk write — when it changed nothing, which is what a
/// round between two devices that already agree costs.
///
/// The single way a record from ANOTHER device enters this Core's directory: the
/// rounds here, and the roster that ends a pairing (`pairing::lan_tail`). Which is
/// why the rules live in [`crate::state::SessionState::absorb`] and not in either
/// caller — a pairing that took a peer's word for something would be a second set
/// of rules, and the weaker one would be the one that mattered.
pub(crate) fn absorb(state: &Arc<AppState>, roster: &Roster) {
    // Both read before the session lock (lock ordering: the transport and
    // `account_root` each have a lock of their own).
    let lan: std::collections::BTreeSet<String> = state.transport.lan_peers().into_iter().collect();
    let ak_pub = state
        .account_root
        .lock()
        .expect("lock account_root")
        .as_ref()
        .map(|root| root.ak_pub.clone());
    let Some(ak_pub) = ak_pub else { return };
    let own_node_id = state.identity.node_id();

    let mut s = state.session.lock().expect("lock session");
    let changed = s.absorb(&ak_pub, &own_node_id, roster);
    if changed.struck_ourselves {
        // The account's own signature, striking THIS device off. Obeyed — outside
        // the session lock, because leaving takes every lock in the book, one at
        // a time. Nothing else came out of that roster (`absorb` short-circuits),
        // so there is nothing to save or announce besides the leave itself.
        drop(s);
        tracing::warn!("the account has struck this device off: leaving the account");
        crate::account_key::leave(state);
        return;
    }
    if changed.is_empty() {
        return;
    }
    // Disk before the notifications, under the lock that mutated the state
    // (state-then-disk, like `session.json`): a component told a device exists —
    // or is gone — can rely on a restart still knowing it. `save_unrefreshed`
    // rather than `save`: an exchange between two siblings is not the authority
    // the store's freshness bound is about, and must not hold that bound open.
    if !changed.barred.is_empty() {
        crate::directory::save_revoked(&state.config_dir, &s.revoked);
    }
    if let Some(devices) = s.devices.as_ref() {
        crate::directory::save_unrefreshed(&state.config_dir, devices);
    }
    let own = s.own_device_id.clone();
    let server_connected = s.server_connected;
    // Under the session lock (order: session then registry), like every directory
    // broadcast: the order of notifications is the order of states.
    let registry = state.registry.lock().expect("lock registry");
    for (method, records) in [
        ("device.added", &changed.added),
        ("device.updated", &changed.updated),
    ] {
        for record in records {
            let device = enrich_device(record, own.as_deref(), server_connected, &lan);
            registry.notify_topic("devices", method, &json!({ "device": device }));
        }
    }
    for device_id in &changed.removed {
        registry.notify_topic(
            "devices",
            "device.removed",
            &json!({ "device_id": device_id }),
        );
    }
}

/// Bounds an I/O future: beyond `dur`, a `TimedOut` error rather than a task that
/// waits on a peer forever.
async fn timed<T>(
    dur: Duration,
    fut: impl Future<Output = std::io::Result<T>>,
    what: &str,
) -> std::io::Result<T> {
    match tokio::time::timeout(dur, fut).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("timed out: {what}"),
        )),
    }
}
