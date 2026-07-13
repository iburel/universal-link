// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Server session: a persistent WebSocket connection to the server, carried by
//! a single task — setup (challenge → authenticate → directory snapshot),
//! upkeep of the device cache, relaying `device.*` notifications to the IPC
//! topics, proxies, reconnection with backoff (doc/server-api.md, "Connection
//! lifecycle").
//!
//! Unlike the IPC, no read/write separation: the server is the Core's trusted
//! peer, the outbound traffic is tiny, and the unsplit stream lets tungstenite
//! answer pings on its own.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::connector::IoStream;
use crate::rpc::RpcErr;
use crate::state::{AppState, ServerCmd, enrich_device};

/// The server connection, whatever connector opened it (cleartext or TLS).
/// Boxed rather than generic: without it the stream parameter would contaminate
/// `ServerConn`, `request`, `ws_request` and their callers.
pub(crate) type ServerWs = WebSocketStream<Box<dyn IoStream>>;

/// Opens the stream (TCP, plus TLS if the scheme requires it) then carries out
/// the WebSocket handshake over it. The URL serves twice: the target to reach,
/// and the `Host` header that tungstenite derives from it.
pub(crate) async fn open_ws(state: &AppState, url: &str) -> Result<ServerWs, String> {
    let location =
        crate::connector::parse_url(url).ok_or_else(|| format!("unreadable URL: {url}"))?;
    let stream = state
        .connector
        .connect(&location.target)
        .await
        .map_err(|e| format!("connecting to {}: {e}", location.authority))?;
    let (ws, _) = tokio_tungstenite::client_async(url, stream)
        .await
        .map_err(|e| format!("WebSocket handshake: {e}"))?;
    Ok(ws)
}

/// Cap of the exponential reconnection backoff.
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);
/// A setup (connection + auth + snapshot) that drags on beyond this counts as
/// a failed attempt — without it, a mute server would freeze reconnection.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Beyond this, a write that makes no progress counts as a dead connection
/// (same policy as everywhere else in the project).
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Depth of the proxied-request queue.
const CMD_QUEUE_DEPTH: usize = 32;
/// Beyond this, a request proxied to the server counts as a lost server — no
/// caller stays suspended on a server that no longer responds.
const PROXY_TIMEOUT: Duration = Duration::from_secs(10);
/// Re-check cadence for the data plane relay: a relay still unknown is retried,
/// a relay that changes is republished. The probe itself is cheap (the online
/// endpoint responds immediately).
const RELAY_RECHECK: Duration = Duration::from_secs(30);

/// Passes a request to the session task and awaits its reply. Disconnected,
/// queue full, task fallen along the way or too slow: `SERVER_UNREACHABLE` —
/// never an unbounded wait on the caller (IPC loop, re-auth flow).
pub(crate) async fn proxy(
    state: &AppState,
    method: &'static str,
    params: Value,
) -> Result<Value, RpcErr> {
    let unreachable = || RpcErr::app("SERVER_UNREACHABLE");
    let tx = {
        let s = state.session.lock().expect("lock session");
        s.server_tx.clone().ok_or_else(unreachable)?
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.try_send(ServerCmd {
        method,
        params,
        reply: reply_tx,
    })
    .map_err(|_| unreachable())?;
    match tokio::time::timeout(PROXY_TIMEOUT, reply_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) | Err(_) => Err(unreachable()),
    }
}

/// The contents of `session.json` — written by login (building block 3), read
/// at startup. Its presence counts as `logged_in`.
pub struct SessionInfo {
    pub server_url: String,
    pub device_id: String,
    /// Opaque JSON (the account's identity), replayed as-is in the statuses.
    pub account: Option<Value>,
}

/// Reads `session.json`; absent → no session. Corrupt → same, but we say so:
/// the daemon must start anyway (the IPC stays the diagnostic channel).
pub fn read_session_file(config_dir: &Path) -> Option<SessionInfo> {
    let path = config_dir.join("session.json");
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    // An empty file is not an anomaly: it is the trace of a logout whose
    // deletion had failed (see `remove_session_file`).
    if text.trim().is_empty() {
        return None;
    }
    let parsed: Option<SessionInfo> = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
        Some(SessionInfo {
            server_url: v.get("server_url")?.as_str()?.to_string(),
            device_id: v.get("device_id")?.as_str()?.to_string(),
            account: v.get("account").filter(|a| !a.is_null()).cloned(),
        })
    });
    if parsed.is_none() {
        tracing::error!(path = %path.display(), "unreadable session.json: session ignored");
    }
    parsed
}

enum Outcome {
    /// The server no longer recognizes this device: the session is dead,
    /// enrollment must be redone. Reconnecting in a loop would be pointless.
    Fatal,
    /// The session was closed under our feet (logout): terminate without
    /// touching the state — logout has already said everything.
    Stop,
    /// Attempt or connection ended: we will retry.
    Retry { was_connected: bool },
}

pub async fn run(state: Arc<AppState>, info: SessionInfo) {
    let base_delay = state.reconnect_base_delay;
    let mut delay = base_delay;
    loop {
        match connect_and_serve(&state, &info).await {
            Outcome::Fatal => {
                drop_session(&state);
                return;
            }
            Outcome::Stop => return,
            Outcome::Retry { was_connected } => {
                mark_disconnected(&state);
                if was_connected {
                    // An established connection that drops is retried quickly;
                    // the backoff only punishes consecutive failures.
                    delay = base_delay;
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RECONNECT_MAX_DELAY);
            }
        }
    }
}

/// The session no longer holds (revocation…): forget everything, including on
/// disk, and notify the subscribers. The state returns to that of a Core never
/// logged in. No effect if a logout has already been through here.
fn drop_session(state: &AppState) {
    {
        let mut s = state.session.lock().expect("lock session");
        if !s.logged_in {
            return;
        }
        let (payload, _abort) = s.forget();
        // The refresh token belonged to this session: a device the account has
        // struck off should no longer hold the means to obtain ID tokens.
        state.secrets.delete(crate::secrets::REFRESH_TOKEN);
        remove_session_file(&state.config_dir);
        // The broadcast goes out under the session lock (order: session then
        // registry) — the order of notifications is the order of transitions.
        state.registry.lock().expect("lock registry").notify_topic(
            "session",
            "session.changed",
            &payload,
        );
    }
    // A pending re-auth flow belonged to the session: it dies with it.
    if let Some(slot) = state.login.lock().expect("lock login").take() {
        slot.abort.abort();
    }
}

/// Removes `session.json` — and does not fail silently: this file is exactly
/// what counts as `logged_in` at the next startup. If the deletion fails (file
/// held open on Windows…), emptying it has the same effect: an empty file
/// counts as "no session".
pub(crate) fn remove_session_file(config_dir: &Path) {
    let path = config_dir.join("session.json");
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::error!(error = %e, "failed to remove session.json: contents emptied instead");
            if let Err(e) = crate::write_private_file(&path, "") {
                tracing::error!(error = %e, "failed to erase session.json");
            }
        }
    }
}

/// The connection dropped: the session remains, the freshness does not. Only
/// notifies the transition (a series of failed attempts is silent).
fn mark_disconnected(state: &AppState) {
    let mut s = state.session.lock().expect("lock session");
    s.server_tx = None;
    if !s.server_connected {
        return;
    }
    s.server_connected = false;
    let payload = s.status_record();
    // Broadcast under the session lock (order: session then registry): an
    // interleaved logout cannot have its notification duplicated by this one.
    state.registry.lock().expect("lock registry").notify_topic(
        "session",
        "session.changed",
        &payload,
    );
}

struct ServerConn {
    ws: ServerWs,
    next_id: u64,
    /// Notifications received during setup, to be replayed on the cache once
    /// primed: the server may emit one AFTER building the snapshot but BEFORE
    /// enqueuing its reply — dropping it would leave a hole. The reverse replay
    /// may re-apply an event already covered by the snapshot (a window of a few
    /// instructions on the server side, resolved at the device's next event) —
    /// the hole, by contrast, would persist.
    buffered: Vec<(String, Value)>,
}

enum SetupErr {
    Transport,
    Rpc(RpcErr),
}

/// Expected proxied replies: id → (method and params sent, reply channel). The
/// params stay: a revoke's reply does not carry the device_id, it is the
/// request that gives it.
type PendingReplies = HashMap<u64, (&'static str, Value, oneshot::Sender<Result<Value, RpcErr>>)>;

impl ServerConn {
    /// A sequential request of the setup phase. The notifications received in
    /// the meantime are set aside (see `buffered`).
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, SetupErr> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.ws.send(Message::text(msg.to_string())).await.is_err() {
            return Err(SetupErr::Transport);
        }
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let Ok(v) = serde_json::from_str::<Value>(&t) else {
                        return Err(SetupErr::Transport);
                    };
                    if let Some(m) = v.get("method").and_then(Value::as_str) {
                        let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
                        self.buffered.push((m.to_string(), params));
                        continue;
                    }
                    if v.get("id") == Some(&json!(id)) {
                        if let Some(err) = v.get("error") {
                            return Err(SetupErr::Rpc(RpcErr::from_value(err)));
                        }
                        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    return Err(SetupErr::Transport);
                }
                Some(Ok(_)) => {}
            }
        }
    }
}

async fn connect_and_serve(state: &AppState, info: &SessionInfo) -> Outcome {
    // The timeout covers opening the stream AND the handshake: a connector that
    // drags (TLS to a mute peer) would otherwise freeze reconnection.
    let connected = tokio::time::timeout(SETUP_TIMEOUT, open_ws(state, &info.server_url)).await;
    let ws = match connected {
        Ok(Ok(ws)) => ws,
        _ => {
            return Outcome::Retry {
                was_connected: false,
            };
        }
    };
    let mut conn = ServerConn {
        ws,
        next_id: 0,
        buffered: Vec::new(),
    };

    // Our account attestation (C7), if this device joined the account. The
    // server stores it opaquely and forgets it on restart (in-memory state): we
    // republish it on EVERY (re)connection, before the snapshot — so our own
    // record comes back already carrying it, and peers receive it via the
    // `device.updated` the server broadcasts.
    let attestation = state
        .account_root
        .lock()
        .expect("lock account_root")
        .as_ref()
        .map(|root| root.attestation.clone());
    let setup = tokio::time::timeout(SETUP_TIMEOUT, async {
        let challenge = conn.request("auth.challenge", json!({})).await?;
        let nonce = challenge["nonce"].as_str().ok_or(SetupErr::Transport)?;
        let proof = state.identity.proof(nonce);
        conn.request(
            "auth.authenticate",
            json!({ "device_id": info.device_id, "proof": proof }),
        )
        .await?;
        if let Some(attestation) = &attestation {
            conn.request("presence.update", json!({ "attestation": attestation }))
                .await?;
        }
        // The snapshot BEFORE announcing the connection: connected ⇒ cache primed.
        conn.request("devices.list", json!({})).await
    })
    .await;
    let list = match setup {
        Ok(Ok(list)) => list,
        Ok(Err(SetupErr::Rpc(err)))
            if matches!(
                err.app.as_deref(),
                Some("DEVICE_REVOKED" | "DEVICE_UNKNOWN")
            ) =>
        {
            return Outcome::Fatal;
        }
        _ => {
            return Outcome::Retry {
                was_connected: false,
            };
        }
    };
    let Some(items) = list.as_array() else {
        return Outcome::Retry {
            was_connected: false,
        };
    };
    let mut devices = BTreeMap::new();
    for d in items {
        if let Some(id) = d.get("device_id").and_then(Value::as_str) {
            devices.insert(id.to_string(), d.clone());
        }
    }

    let (tx, mut cmd_rx) = mpsc::channel::<ServerCmd>(CMD_QUEUE_DEPTH);
    {
        let mut s = state.session.lock().expect("lock session");
        // A logout may have passed during setup: its abort() only bites at the
        // next await, and this block is synchronous. Republishing the connected
        // state would resurrect an already-forgotten session — permanently,
        // since nothing more would come to correct it.
        if !s.logged_in {
            return Outcome::Stop;
        }
        s.server_connected = true;
        s.devices = Some(devices);
        s.server_tx = Some(tx);
        let payload = s.status_record();
        // Broadcast under the session lock (order: session then registry):
        // otherwise a full logout could interleave here and its notification go
        // out BEFORE this one — subscribers would stay on "connected".
        state.registry.lock().expect("lock registry").notify_topic(
            "session",
            "session.changed",
            &payload,
        );
    }
    // Cache primed: replay what setup set aside.
    for (method, params) in std::mem::take(&mut conn.buffered) {
        apply_event(state, &method, &params);
    }

    // Cruising regime: relaying notifications, proxies, detecting the end of
    // the connection. `pending` retains the method: a rename's reply carries
    // the record to apply to the cache HERE, in the order of the server flow —
    // not in the IPC task, which could apply it after a more recent
    // `device.offline` (or never, if its timeout expired).
    let mut pending: PendingReplies = HashMap::new();
    // Publish our data plane relay as soon as iroh knows it: peers need it to
    // reach us (`node_id` + `relay_url` from the directory = their only
    // discovery). A `Pin<Box<dyn Future>>` is directly pollable in the select;
    // its discovery can drag on the iroh side, so we do not let it hold up the
    // cruising loop.
    //
    // The probe is RE-ARMED after each resolution (never re-polled once
    // resolved): a relay still unknown on the first pass (`None` — iroh can
    // take more than ten seconds offline) will be retried, a relay that CHANGES
    // during the session will be republished. The server, for its part, forgets
    // our relay when the connection drops: every connection — this one as well
    // as the reconnections — starts from scratch (`published_relay = None`) and
    // republishes.
    //
    // Publication is only considered acquired (latched) at the server's REPLY:
    // a rejected `presence.update` (e.g. `RATE_LIMITED`, applied to all methods)
    // un-latches to retry at the next probe — without this, a write that
    // succeeded but was refused would leave the device unreachable for the whole
    // session, silently. The reply is routed by `pending` then forwarded into
    // `relay_ack` (a bridging task, because a oneshot does not `select!` cleanly
    // as an option).
    let mut relay_probe: crate::dataplane::HomeRelay<'_> = state.transport.home_relay();
    let mut published_relay: Option<String> = None;
    let (relay_ack_tx, mut relay_ack_rx) = mpsc::channel::<(String, bool)>(4);
    loop {
        tokio::select! {
            relay_url = &mut relay_probe => {
                if let Some(url) =
                    relay_url.filter(|url| published_relay.as_deref() != Some(url.as_str()))
                {
                    conn.next_id += 1;
                    let id = conn.next_id;
                    let msg = json!({
                        "jsonrpc": "2.0", "id": id,
                        "method": "presence.update", "params": { "relay_url": url },
                    });
                    let sent = tokio::time::timeout(
                        WRITE_TIMEOUT,
                        conn.ws.send(Message::text(msg.to_string())),
                    )
                    .await;
                    if !matches!(sent, Ok(Ok(()))) {
                        return Outcome::Retry { was_connected: true };
                    }
                    // OPTIMISTIC latch: we do not resend until we have the
                    // reply; the eventual failure will lift it. Our own cache
                    // record carries the relay (the server does not send the
                    // publisher its `device.updated`, and a cache that ignores
                    // itself would fail a local → local ping).
                    set_own_relay(state, &info.device_id, &url);
                    published_relay = Some(url.clone());
                    let (reply_tx, reply_rx) = oneshot::channel();
                    pending.insert(id, ("presence.update", Value::Null, reply_tx));
                    let ack_tx = relay_ack_tx.clone();
                    tokio::spawn(async move {
                        let accepted = matches!(reply_rx.await, Ok(Ok(_)));
                        let _ = ack_tx.send((url, accepted)).await;
                    });
                }
                relay_probe = Box::pin(async {
                    tokio::time::sleep(RELAY_RECHECK).await;
                    state.transport.home_relay().await
                });
            }
            Some((url, accepted)) = relay_ack_rx.recv() => {
                // Rejection (and still the relay we thought published): we
                // un-latch to republish at the next probe.
                if !accepted && published_relay.as_deref() == Some(url.as_str()) {
                    tracing::warn!(relay = %url, "presence.update refused: republication planned");
                    published_relay = None;
                }
            }
            msg = conn.ws.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    // Tolerant JSON: an unintelligible message is ignored.
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        handle_server_message(state, v, &mut pending);
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    // A revoked device is not notified by message: the close
                    // reason is authoritative (doc/server-api.md).
                    if frame.as_ref().is_some_and(|f| f.reason.as_str() == "DEVICE_REVOKED") {
                        return Outcome::Fatal;
                    }
                    // `REPLACED` included: we reconnect — if another process
                    // usurps the identity, it is the one that is the anomaly.
                    return Outcome::Retry { was_connected: true };
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return Outcome::Retry { was_connected: true },
            },
            cmd = cmd_rx.recv() => {
                // All senders dropped: the state no longer references us.
                let Some(cmd) = cmd else {
                    return Outcome::Retry { was_connected: true };
                };
                conn.next_id += 1;
                let id = conn.next_id;
                let msg = json!({
                    "jsonrpc": "2.0", "id": id,
                    "method": cmd.method, "params": cmd.params.clone(),
                });
                // Bounded write: a server that no longer reads without closing
                // (TCP buffer full) must not freeze the loop — without it, the
                // server heartbeat would be frozen along with it.
                let sent = tokio::time::timeout(
                    WRITE_TIMEOUT,
                    conn.ws.send(Message::text(msg.to_string())),
                )
                .await;
                if !matches!(sent, Ok(Ok(()))) {
                    let _ = cmd.reply.send(Err(RpcErr::app("SERVER_UNREACHABLE")));
                    return Outcome::Retry { was_connected: true };
                }
                pending.insert(id, (cmd.method, cmd.params, cmd.reply));
            }
        }
    }
}

/// A message from the server in the cruising regime: a notification to apply
/// and relay, or a reply to a proxied request.
fn handle_server_message(state: &AppState, v: Value, pending: &mut PendingReplies) {
    if let Some(method) = v.get("method").and_then(Value::as_str) {
        let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
        apply_event(state, method, &params);
        return;
    }
    let Some(id) = v.get("id").and_then(Value::as_u64) else {
        return;
    };
    let Some((method, params, reply)) = pending.remove(&id) else {
        return;
    };
    let result = match v.get("error") {
        Some(err) => Err(RpcErr::from_value(err)),
        None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
    };
    // The server excludes the requester from its broadcast: the reply is the
    // only trace of the event — cache + subscribers, right here, in the order
    // of the server flow, whether or not the IPC task is still there for the
    // reply.
    if let Ok(result) = &result {
        match method {
            "devices.rename" => {
                if let Some(record) = result.get("device") {
                    apply_event(state, "device.updated", &json!({ "device": record }));
                }
            }
            "devices.revoke" => {
                if let Some(device_id) = params.get("device_id") {
                    apply_event(state, "device.removed", &json!({ "device_id": device_id }));
                }
            }
            _ => {}
        }
    }
    let _ = reply.send(result);
}

/// Applies a `device.*` to the cache and relays it to the subscribers of the
/// `devices` topic, records enriched with `is_self`. An event we do not know
/// how to apply is not relayed: we do not broadcast a state we do not hold.
fn apply_event(state: &AppState, method: &str, params: &Value) {
    let relayed = {
        let mut s = state.session.lock().expect("lock session");
        let own = s.own_device_id.clone();
        let Some(devices) = &mut s.devices else {
            return;
        };
        match method {
            "device.added" | "device.online" | "device.updated" => {
                let Some(record) = params.get("device") else {
                    return;
                };
                let Some(id) = record.get("device_id").and_then(Value::as_str) else {
                    return;
                };
                devices.insert(id.to_string(), record.clone());
                json!({ "device": enrich_device(record, own.as_deref()) })
            }
            "device.offline" => {
                let Some(id) = params.get("device_id").and_then(Value::as_str) else {
                    return;
                };
                if let Some(record) = devices.get_mut(id) {
                    record["online"] = json!(false);
                    if let Some(seen) = params.get("last_seen") {
                        record["last_seen"] = seen.clone();
                    }
                }
                params.clone()
            }
            "device.removed" => {
                let Some(id) = params.get("device_id").and_then(Value::as_str) else {
                    return;
                };
                devices.remove(id);
                params.clone()
            }
            _ => return,
        }
    };
    state
        .registry
        .lock()
        .expect("lock registry")
        .notify_topic("devices", method, &relayed);
}

/// Carries our freshly published `relay_url` onto our own cache record. The
/// server does not send the publisher its `device.updated`: without this
/// gesture, the local directory would be the only one in the account to ignore
/// our relay.
fn set_own_relay(state: &AppState, device_id: &str, url: &str) {
    let mut s = state.session.lock().expect("lock session");
    if let Some(record) = s
        .devices
        .as_mut()
        .and_then(|devices| devices.get_mut(device_id))
    {
        record["relay_url"] = json!(url);
    }
}

/// Carries our freshly published attestation (C7) onto our own cache record —
/// same reason as `set_own_relay`: the server excludes the publisher from its
/// broadcast. Called after an `account.setup`/`join` during a session (on
/// (re)connection, the post-publication snapshot already carries the
/// attestation).
pub(crate) fn set_own_attestation(state: &AppState, attestation: &str) {
    let mut s = state.session.lock().expect("lock session");
    let Some(device_id) = s.own_device_id.clone() else {
        return;
    };
    if let Some(record) = s
        .devices
        .as_mut()
        .and_then(|devices| devices.get_mut(&device_id))
    {
        record["attestation"] = json!(attestation);
    }
}
