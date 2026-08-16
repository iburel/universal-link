// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Peer messages between same-role components (doc/core-api.md, "peers.*";
//! ticket #83): the control-plane counterpart of what `dir_sync` does
//! internally, exposed as an extension point.
//!
//! `peers.send { device_id, payload }` carries an OPAQUE, capped payload to
//! the component holding the SAME role on another device of the account. The
//! Core guarantees: the target is an attested device (C7, verified before any
//! dial), the transport is the encrypted data plane, and the routing is
//! strictly role-to-same-role - the sender's role is stamped by its Core,
//! never chosen. Everything else is the components' own dialect: the Core
//! caches nothing and interprets nothing.
//!
//! The send is one bounded round-trip (`peer_msg` out, `peer_ack` back), so
//! `COMPONENT_ABSENT` is the remote Core's word rather than a guess - but
//! delivery stays best-effort by design: an ack only says the notification was
//! queued to at least one matching component, the sender retries on its own
//! schedule, and the `devices` topic already tells it when a peer becomes
//! reachable.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::time::{Duration, timeout};

use crate::connector::IoStream;
use crate::dataplane;
use crate::rpc::RpcErr;
use crate::state::AppState;

/// Cap on a serialized `payload` (doc/core-api.md: tens of KiB). Enforced at
/// the RPC boundary - its own honest code, rather than letting callers
/// discover the data plane's frame limit - and defensively on arrival.
pub(crate) const PEER_MSG_MAX: usize = 64 * 1024;

/// Whole-exchange budget (connect + one frame each way): the house exchange
/// norm. Beyond it the peer is, for the caller's purposes, unreachable.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the receiver holds the stream after its ack, so the ack is not
/// abandoned in flight on the QUIC side.
const LINGER: Duration = Duration::from_secs(5);

/// Sends `payload` to the components holding `role` on `device_id`. The
/// errors follow `files.send`'s targeting doctrine: `DEVICE_UNKNOWN` (absent
/// or attested under a foreign key - indistinguishable, fail-closed),
/// `DEVICE_OFFLINE` (no route known, or nothing usable answered in time - a
/// route is an address claim, not liveness), `COMPONENT_ABSENT` (the device
/// answered: nobody there holds the role with the receiving scope).
pub(crate) async fn send(
    state: &Arc<AppState>,
    role: &str,
    device_id: &str,
    payload: Value,
) -> Result<Value, RpcErr> {
    let serialized = serde_json::to_vec(&payload).map_err(|_| RpcErr::invalid_params("payload"))?;
    if serialized.len() > PEER_MSG_MAX {
        return Err(RpcErr::app("PAYLOAD_TOO_LARGE"));
    }
    let peer =
        dataplane::resolve_peer(state, device_id).ok_or_else(|| RpcErr::app("DEVICE_UNKNOWN"))?;
    if !dataplane::peer_reachable(state, &peer) {
        return Err(RpcErr::app("DEVICE_OFFLINE"));
    }
    let delivered = timeout(SEND_TIMEOUT, exchange(state, &peer, role, &payload))
        .await
        .map_err(|_| RpcErr::app("DEVICE_OFFLINE"))?
        .map_err(|_| RpcErr::app("DEVICE_OFFLINE"))?;
    if !delivered {
        return Err(RpcErr::app("COMPONENT_ABSENT"));
    }
    Ok(json!({}))
}

/// One `peer_msg` round-trip. Any answer that is not a `peer_ack` - the
/// tombstone a struck-off dialer is served, a frame from another era - fails
/// the protocol check gracefully (the caller words it as unreachable).
async fn exchange(
    state: &Arc<AppState>,
    peer: &dataplane::PeerAddr,
    role: &str,
    payload: &Value,
) -> std::io::Result<bool> {
    let mut stream = state.transport.open(peer).await?;
    let frame = serde_json::to_vec(&json!({
        "type": "peer_msg",
        "role": role,
        "payload": payload,
    }))?;
    dataplane::write_frame(&mut stream, &frame).await?;
    let reply = dataplane::read_frame(&mut stream).await?;
    // Initiator closes after reading the ack (QUIC lifecycle).
    let _ = stream.shutdown().await;
    let v: Value =
        serde_json::from_slice(&reply).map_err(|_| crate::datachannel::unexpected("peer_ack"))?;
    if v.get("type").and_then(Value::as_str) != Some("peer_ack") {
        return Err(crate::datachannel::unexpected("peer_ack"));
    }
    Ok(v.get("delivered").and_then(Value::as_bool).unwrap_or(false))
}

/// Receiver side of a `peer_msg`: delivers `peer.message` to every active
/// connection holding the sender's role AND the `peers.message` scope, then
/// acks whether anyone was there. The peer is already attested (C7 ran in
/// `serve`); the sender is named by OUR directory (`device_id_for` on the
/// authenticated node), never by its own claim.
pub(crate) async fn recv(
    state: Arc<AppState>,
    peer_node_id: String,
    first: Value,
    mut stream: Box<dyn IoStream>,
) {
    let delivered = deliver(&state, &peer_node_id, &first);
    let ack = serde_json::to_vec(&json!({ "type": "peer_ack", "delivered": delivered }))
        .expect("serialize peer_ack");
    // No-progress bound on the reply, like every responder: a peer that never
    // grants credit must not pin this handler's slot.
    let _ = crate::datachannel::bounded(dataplane::write_frame(&mut stream, &ack)).await;
    let _ = stream.shutdown().await;
    let _ = timeout(LINGER, dataplane::drain(&mut stream)).await;
}

/// Validates the frame and pushes the notification. `false` for anything
/// malformed (a compliant sender's Core cannot produce it) and for a role
/// nobody holds here: the two collapse deliberately - the ack owes the sender
/// no diagnosis of this device's components.
fn deliver(state: &AppState, peer_node_id: &str, first: &Value) -> bool {
    let Some(role) = first.get("role").and_then(Value::as_str) else {
        return false;
    };
    let Some(payload) = first.get("payload") else {
        return false;
    };
    // Defensive mirror of the sender-side cap.
    match serde_json::to_vec(payload) {
        Ok(b) if b.len() <= PEER_MSG_MAX => {}
        _ => return false,
    }
    let device_id =
        dataplane::device_id_for(state, peer_node_id).unwrap_or_else(|| peer_node_id.to_string());
    let params = json!({ "device_id": device_id, "payload": payload });
    // Session-derived naming above, registry push below: never both locks at
    // once (lock ordering).
    state
        .registry
        .lock()
        .expect("lock registry")
        .notify_role_scope(role, "peers.message", "peer.message", &params)
}
