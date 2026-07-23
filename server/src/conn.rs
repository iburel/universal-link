// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! A WebSocket connection: lifecycle, heartbeat, method dispatch.
//!
//! **Reads and writes are separated.** The main loop only reads the socket and
//! beats the heartbeat; all writes (responses, notifications, closes) go
//! through a bounded queue drained by a dedicated write task. Two properties
//! follow from this:
//!
//! - a client that stops reading cannot freeze the loop (it never waits on the
//!   socket): the queue fills up, the send fails, the connection is closed —
//!   per-connection memory is bounded and the heartbeat stays alive;
//! - the queue is FIFO, so a response enqueued before a close leaves before it
//!   (the self-revocation case).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use ed25519_dalek::{Signature, VerifyingKey};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::API_VERSION;
use crate::rpc::{self, RpcErr};
use crate::state::{AppState, ConnId, DeviceEntry, OutMsg, now_rfc3339, random_hex};

const CLOSE_REPLACED: &str = "REPLACED";
const CLOSE_REVOKED: &str = "DEVICE_REVOKED";
const CLOSE_HEARTBEAT: &str = "HEARTBEAT_LOST";

/// Write queue depth. Beyond this, the consumer is too slow: we disconnect it
/// rather than accumulate (it will resync on reconnect).
const OUT_QUEUE_DEPTH: usize = 256;
/// Beyond this, a write that makes no progress counts as a dead connection.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds on the fields stored and then rebroadcast in the device record.
const NAME_MAX: usize = 128;
const STATUS_MAX: usize = 128;
const RELAY_URL_MAX: usize = 2048;
/// Account attestation (C7): an OPAQUE blob to the server (Ed25519 signature in
/// hex = 128 chars), stored and rebroadcast without ever being interpreted.
/// Bounded loosely: the server does not decode it, it just rejects the absurd.
const ATTESTATION_MAX: usize = 256;

const PLATFORMS: [&str; 3] = ["windows", "macos", "linux"];

struct RateWindow {
    start: Instant,
    count: u32,
}

struct Conn {
    state: Arc<AppState>,
    conn_id: ConnId,
    /// Sender to its own queue (cloned into the directory on authentication).
    tx: mpsc::Sender<OutMsg>,
    /// Device bound by `auth.authenticate`, and its account.
    device_id: Option<String>,
    account: Option<String>,
    /// Current nonce: bound to the connection, single-use, replaced by every
    /// `auth.challenge`.
    nonce: Option<(String, Instant)>,
    rate: Option<RateWindow>,
    /// A close targeting this very connection: it must leave *after* the
    /// in-flight response (self-revocation), so the loop enqueues it itself.
    pending_close: Option<&'static str>,
}

pub async fn run(state: Arc<AppState>, socket: WebSocket) {
    let (tx, out_rx) = mpsc::channel(OUT_QUEUE_DEPTH);
    let conn_id = state.registry.lock().expect("lock registry").new_conn_id();
    let heartbeat_interval = state.config.heartbeat_interval;
    let max_missed = state.config.heartbeat_max_missed;
    let mut conn = Conn {
        state,
        conn_id,
        tx,
        device_id: None,
        account: None,
        nonce: None,
        rate: None,
        pending_close: None,
    };

    let (sink, mut stream) = socket.split();
    let mut writer = tokio::spawn(write_loop(sink, out_rx));
    let mut heartbeat = tokio::time::interval(heartbeat_interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut missed_pongs: u32 = 0;
    let mut writer_done = false;
    let mut closing = false;

    loop {
        tokio::select! {
            // The write task stops on a sent close or a dead socket.
            _ = &mut writer => {
                writer_done = true;
                break;
            }
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Some(reply) = conn.handle_text(text.as_str()).await
                        && conn.tx.try_send(OutMsg::Notify(reply)).is_err()
                    {
                        break;
                    }
                    if let Some(reason) = conn.pending_close.take() {
                        let _ = conn.tx.try_send(OutMsg::Close(reason));
                        closing = true;
                        break;
                    }
                }
                Some(Ok(Message::Pong(_))) => missed_pongs = 0,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = heartbeat.tick() => {
                if missed_pongs > max_missed {
                    // The connection is dead as far as we're concerned: immediate offline.
                    conn.mark_offline();
                    let _ = conn.tx.try_send(OutMsg::Close(CLOSE_HEARTBEAT));
                    closing = true;
                    break;
                }
                // Queue full or write task dead: we don't buffer.
                if conn.tx.try_send(OutMsg::Ping).is_err() {
                    break;
                }
                missed_pongs += 1;
            }
        }
    }

    conn.mark_offline();
    if closing && !writer_done {
        // Let the close (and what precedes it) leave before aborting.
        let _ = tokio::time::timeout(WRITE_TIMEOUT, &mut writer).await;
    }
    if !writer_done {
        writer.abort();
    }
}

/// Drains the write queue. Any write that makes no progress within the allotted
/// time ends the task — the main loop detects this and exits.
async fn write_loop(mut sink: SplitSink<WebSocket, Message>, mut out_rx: mpsc::Receiver<OutMsg>) {
    while let Some(msg) = out_rx.recv().await {
        let frame = match msg {
            OutMsg::Notify(text) => Message::Text(text.into()),
            OutMsg::Ping => Message::Ping(Vec::new().into()),
            OutMsg::Close(reason) => {
                let frame = Message::Close(Some(CloseFrame {
                    code: 1000,
                    reason: reason.into(),
                }));
                let _ = tokio::time::timeout(WRITE_TIMEOUT, sink.send(frame)).await;
                break;
            }
        };
        if !matches!(
            tokio::time::timeout(WRITE_TIMEOUT, sink.send(frame)).await,
            Ok(Ok(()))
        ) {
            break;
        }
    }
    let _ = sink.close().await;
}

impl Conn {
    /// Handles a text frame; returns the response to send, if any.
    async fn handle_text(&mut self, text: &str) -> Option<String> {
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            return Some(rpc::parse_error());
        };
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        let Some(method) = msg
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Some(rpc::response_err(
                &id.unwrap_or(Value::Null),
                &RpcErr::invalid_request(),
            ));
        };
        // Client notification (no id): the API defines none — ignored.
        let id = id?;

        if let Some(limit) = self.state.config.max_requests_per_minute
            && !self.within_rate_limit(limit)
        {
            return Some(rpc::response_err(&id, &RpcErr::app("RATE_LIMITED")));
        }

        Some(match self.dispatch(&method, &params).await {
            Ok(result) => rpc::response_ok(&id, result),
            Err(err) => rpc::response_err(&id, &err),
        })
    }

    async fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, RpcErr> {
        match method {
            "auth.challenge" => self.auth_challenge(),
            "auth.enroll" => self.auth_enroll(params).await,
            "auth.authenticate" => self.auth_authenticate(params),
            "devices.list" => self.devices_list(),
            "devices.rename" => self.devices_rename(params),
            "devices.revoke" => self.devices_revoke(params).await,
            "presence.update" => self.presence_update(params),
            _ => Err(RpcErr::method_not_found(method)),
        }
    }

    fn within_rate_limit(&mut self, limit: u32) -> bool {
        let now = Instant::now();
        let window = self.rate.get_or_insert(RateWindow {
            start: now,
            count: 0,
        });
        if now.duration_since(window.start) > Duration::from_secs(60) {
            window.start = now;
            window.count = 0;
        }
        window.count += 1;
        window.count <= limit
    }

    fn require_account(&self) -> Result<String, RpcErr> {
        self.account
            .clone()
            .ok_or_else(|| RpcErr::app("NOT_AUTHENTICATED"))
    }

    /// Consumes the connection's nonce and verifies the proof of possession.
    fn verify_proof(&mut self, node_id_hex: &str, proof_hex: &str) -> Result<(), RpcErr> {
        let invalid = || RpcErr::app("INVALID_PROOF");
        let (nonce, expires_at) = self.nonce.take().ok_or_else(invalid)?;
        if Instant::now() > expires_at {
            return Err(invalid());
        }
        let key_bytes: [u8; 32] = hex::decode(node_id_hex)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(invalid)?;
        let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| invalid())?;
        let sig_bytes: [u8; 64] = hex::decode(proof_hex)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(invalid)?;
        let signature = Signature::from_bytes(&sig_bytes);
        key.verify_strict(nonce.as_bytes(), &signature)
            .map_err(|_| invalid())
    }

    fn auth_challenge(&mut self) -> Result<Value, RpcErr> {
        let nonce = random_hex(32);
        self.nonce = Some((nonce.clone(), Instant::now() + self.state.config.nonce_ttl));
        Ok(json!({ "nonce": nonce }))
    }

    async fn auth_enroll(&mut self, params: &Value) -> Result<Value, RpcErr> {
        let id_token = rpc::required_str(params, "id_token")?;
        let node_id = rpc::required_str(params, "node_id")?;
        let name = rpc::required_str_max(params, "name", NAME_MAX)?;
        let platform = rpc::required_str(params, "platform")?;
        let proof = rpc::required_str(params, "proof")?;
        if !PLATFORMS.contains(&platform.as_str()) {
            return Err(RpcErr::invalid_params("platform"));
        }

        // Proof of possession BEFORE token validation: bounded local check
        // first (the nonce clock must not run against the JWKS fetch, an
        // unbounded network call), and a bad proof is rejected without
        // bothering the IdP.
        self.verify_proof(&node_id, &proof)?;

        // Sensitive operation: a fresh token is required.
        let claims = self
            .state
            .oidc
            .validate_fresh(&id_token)
            .await
            .map_err(|_| RpcErr::app("OIDC_INVALID"))?;

        let device_id = format!("d_{}", random_hex(8));
        let entry = DeviceEntry {
            account: claims.sub,
            device_id: device_id.clone(),
            name,
            platform,
            node_id,
            relay_url: None,
            status: None,
            last_seen: None,
            // Published separately (`presence.update`) once the device has joined
            // the account (C7): OIDC enrollment alone does not carry it.
            attestation: None,
            conn: None,
        };
        let record = entry.record();
        let account = entry.account.clone();

        {
            let mut reg = self.state.registry.lock().expect("lock registry");
            reg.devices.insert(device_id.clone(), entry);
            reg.broadcast(
                &account,
                self.conn_id,
                "device.added",
                json!({ "device": record }),
            );
        }
        // New device in the directory: to be persisted so it survives a restart
        // (otherwise an OIDC re-login would be forced).
        self.state.persist();

        Ok(json!({ "device_id": device_id, "api_version": API_VERSION, "device": record }))
    }

    fn auth_authenticate(&mut self, params: &Value) -> Result<Value, RpcErr> {
        let device_id = rpc::required_str(params, "device_id")?;
        let proof = rpc::required_str(params, "proof")?;
        let relay_url = rpc::optional_str_max(params, "relay_url", RELAY_URL_MAX)?;

        // A revoked id stays recognized as such: DEVICE_REVOKED ≠ DEVICE_UNKNOWN.
        let node_id = {
            let reg = self.state.registry.lock().expect("lock registry");
            if reg.revoked.contains(&device_id) {
                return Err(RpcErr::app("DEVICE_REVOKED"));
            }
            let entry = reg
                .devices
                .get(&device_id)
                .ok_or_else(|| RpcErr::app("DEVICE_UNKNOWN"))?;
            entry.node_id.clone()
        };
        self.verify_proof(&node_id, &proof)?;

        // Re-binding to a different device: the old one goes offline cleanly.
        if self.device_id.as_deref().is_some_and(|d| d != device_id) {
            self.mark_offline();
        }

        let (record, account) = {
            let mut reg = self.state.registry.lock().expect("lock registry");
            if reg.revoked.contains(&device_id) {
                return Err(RpcErr::app("DEVICE_REVOKED"));
            }
            let Some(entry) = reg.devices.get_mut(&device_id) else {
                return Err(RpcErr::app("DEVICE_UNKNOWN"));
            };
            // One device = at most one connection: the new one replaces the old,
            // closed without a device.offline (no flap).
            let previous = entry.conn.replace((self.conn_id, self.tx.clone()));
            if let Some((old_id, old_tx)) = previous
                && old_id != self.conn_id
            {
                let _ = old_tx.try_send(OutMsg::Close(CLOSE_REPLACED));
            }
            if relay_url.is_some() {
                entry.relay_url = relay_url;
            }
            entry.last_seen = Some(now_rfc3339());
            let record = entry.record();
            let account = entry.account.clone();
            reg.broadcast(
                &account,
                self.conn_id,
                "device.online",
                json!({ "device": record }),
            );
            (record, account)
        };

        self.device_id = Some(device_id);
        self.account = Some(account);
        Ok(json!({ "api_version": API_VERSION, "device": record }))
    }

    fn devices_list(&self) -> Result<Value, RpcErr> {
        let account = self.require_account()?;
        let reg = self.state.registry.lock().expect("lock registry");
        Ok(Value::Array(
            reg.account_devices(&account)
                .map(DeviceEntry::record)
                .collect(),
        ))
    }

    fn devices_rename(&self, params: &Value) -> Result<Value, RpcErr> {
        let account = self.require_account()?;
        let device_id = rpc::required_str(params, "device_id")?;
        let name = rpc::required_str_max(params, "name", NAME_MAX)?;

        let record = {
            let mut reg = self.state.registry.lock().expect("lock registry");
            // The directory is scoped to the account: an id from another account is unknown.
            let Some(entry) = reg
                .devices
                .get_mut(&device_id)
                .filter(|d| d.account == account)
            else {
                return Err(RpcErr::app("DEVICE_UNKNOWN"));
            };
            entry.name = name;
            let record = entry.record();
            reg.broadcast(
                &account,
                self.conn_id,
                "device.updated",
                json!({ "device": record }),
            );
            record
        };
        // The name is durable: to be persisted.
        self.state.persist();
        Ok(json!({ "device": record }))
    }

    async fn devices_revoke(&mut self, params: &Value) -> Result<Value, RpcErr> {
        let account = self.require_account()?;
        let device_id = rpc::required_str(params, "device_id")?;
        let id_token = rpc::required_str(params, "id_token")?;

        // Sensitive operation: a fresh token, belonging to the connection's
        // account (a valid token from another account grants no rights).
        let claims = self
            .state
            .oidc
            .validate_fresh(&id_token)
            .await
            .map_err(|_| RpcErr::app("OIDC_INVALID"))?;
        if claims.sub != account {
            return Err(RpcErr::app("OIDC_INVALID"));
        }

        {
            let mut reg = self.state.registry.lock().expect("lock registry");
            if reg
                .devices
                .get(&device_id)
                .is_none_or(|d| d.account != account)
            {
                return Err(RpcErr::app("DEVICE_UNKNOWN"));
            }
            let entry = reg.devices.remove(&device_id).expect("checked present");
            reg.revoked.insert(device_id.clone());
            // The revoked device is not notified by message: a direct close frame.
            if let Some((revoked_conn_id, revoked_tx)) = entry.conn {
                if revoked_conn_id == self.conn_id {
                    // Self-revocation: the close must follow the response.
                    self.pending_close = Some(CLOSE_REVOKED);
                } else {
                    let _ = revoked_tx.try_send(OutMsg::Close(CLOSE_REVOKED));
                }
            }
            reg.broadcast(
                &account,
                self.conn_id,
                "device.removed",
                json!({ "device_id": device_id }),
            );
        }
        // Removal + revocation are durable: a restart must neither resurrect the
        // device nor forget that it's struck off.
        self.state.persist();
        Ok(json!({}))
    }

    fn presence_update(&self, params: &Value) -> Result<Value, RpcErr> {
        let account = self.require_account()?;
        let status = rpc::optional_str_max(params, "status", STATUS_MAX)?;
        let relay_url = rpc::optional_str_max(params, "relay_url", RELAY_URL_MAX)?;
        // Account attestation (C7): an opaque blob, neither decoded nor verified
        // here — it's the PEER that verifies it under its account key. The server
        // only carries it, blind, like the rest of the directory.
        let attestation = rpc::optional_str_max(params, "attestation", ATTESTATION_MAX)?;
        let device_id = self
            .device_id
            .clone()
            .expect("authenticated → device bound");
        // Only the attestation is durable here (status/relay_url are ephemeral):
        // we persist only if it changes.
        let attestation_changed = attestation.is_some();

        {
            let mut reg = self.state.registry.lock().expect("lock registry");
            let Some(entry) = reg.devices.get_mut(&device_id) else {
                return Err(RpcErr::app("DEVICE_UNKNOWN")); // revoked in the meantime
            };
            // Connection replaced in the meantime (its close frame is on the way):
            // it is no longer the device's presence — ignored, as in `mark_offline`,
            // so as not to overwrite the state published by the current connection
            // nor rebroadcast a stale `device.updated`.
            if entry.conn.as_ref().map(|(id, _)| *id) != Some(self.conn_id) {
                return Ok(json!({}));
            }
            if status.is_some() {
                entry.status = status;
            }
            if relay_url.is_some() {
                entry.relay_url = relay_url;
            }
            if attestation.is_some() {
                entry.attestation = attestation;
            }
            let record = entry.record();
            reg.broadcast(
                &account,
                self.conn_id,
                "device.updated",
                json!({ "device": record }),
            );
        }
        if attestation_changed {
            self.state.persist();
        }
        Ok(json!({}))
    }

    /// If this connection is still its device's current connection, marks it
    /// offline and notifies the account. No effect otherwise (connection
    /// replaced, device revoked, or never authenticated).
    fn mark_offline(&mut self) {
        let Some(device_id) = self.device_id.take() else {
            return;
        };
        self.account = None;
        let mut reg = self.state.registry.lock().expect("lock registry");
        let Some(entry) = reg.devices.get_mut(&device_id) else {
            return;
        };
        if entry.conn.as_ref().map(|(id, _)| *id) != Some(self.conn_id) {
            return;
        }
        entry.conn = None;
        // The dial info dies with the connection: without this, the previous
        // session's relay_url would be served again as current on reconnect
        // (`device.online`, `devices.list`). The device republishes a fresh one
        // via `presence.update` once reconnected.
        entry.relay_url = None;
        let last_seen = now_rfc3339();
        entry.last_seen = Some(last_seen.clone());
        let account = entry.account.clone();
        reg.broadcast(
            &account,
            self.conn_id,
            "device.offline",
            json!({ "device_id": device_id, "last_seen": last_seen }),
        );
    }
}

#[cfg(test)]
mod tests {
    //! The "replaced connection that still speaks" window is a race: the
    //! `REPLACED` close frame leaves as soon as the replacement happens and the
    //! old connection's loop dies right after. The integration suite
    //! (`tests/api`) cannot open this window deterministically, hence a test at
    //! the `Conn` level directly.

    use super::*;
    use crate::store::{DurableState, MemoryStore};
    use crate::{Config, OidcConfig};

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState::new(
            Config {
                bind_addr: "127.0.0.1:0".parse().expect("addr"),
                oidc: OidcConfig {
                    // Never contacted: the JWKS is fetched only on the first OIDC
                    // validation, and `presence.update` performs none.
                    issuer_url: "http://127.0.0.1:9".into(),
                    client_id: "tests".into(),
                    max_fresh_token_age: Duration::from_secs(300),
                    jwks_refresh_min_interval: Duration::from_secs(60),
                },
                heartbeat_interval: Duration::from_secs(30),
                heartbeat_max_missed: 2,
                nonce_ttl: Duration::from_secs(60),
                max_requests_per_minute: None,
            },
            Arc::new(MemoryStore::default()),
            DurableState::default(),
        ))
    }

    /// A `Conn` authenticated as `device_id` on `conn_id`, without a socket
    /// (nothing writes here: sends go through the `mpsc` queues).
    fn conn_bound_to(state: &Arc<AppState>, conn_id: ConnId, device_id: &str) -> Conn {
        let (tx, _rx) = mpsc::channel(8);
        Conn {
            state: state.clone(),
            conn_id,
            tx,
            device_id: Some(device_id.to_string()),
            account: Some("alice".to_string()),
            nonce: None,
            rate: None,
            pending_close: None,
        }
    }

    #[tokio::test]
    async fn presence_update_from_replaced_connection_is_ignored() {
        let state = test_state();
        // The device is online on connection 2 (current); connection 1,
        // replaced, is still alive for as long as its close frame takes to leave.
        let (current_tx, mut current_rx) = mpsc::channel(8);
        state
            .registry
            .lock()
            .expect("lock registry")
            .devices
            .insert(
                "d_test".into(),
                DeviceEntry {
                    account: "alice".into(),
                    device_id: "d_test".into(),
                    name: "PC".into(),
                    platform: "linux".into(),
                    node_id: "00".repeat(32),
                    relay_url: Some("https://relay-fresh.example/".into()),
                    status: Some("ready".into()),
                    last_seen: None,
                    attestation: None,
                    conn: Some((2, current_tx)),
                },
            );

        let replaced = conn_bound_to(&state, 1, "d_test");
        let update = json!({
            "status": "stale",
            "relay_url": "https://relay-stale.example/",
        });
        let result = match replaced.presence_update(&update) {
            Ok(result) => result,
            Err(err) => panic!("silently ignored, not an error: {}", err.message),
        };
        assert_eq!(result, json!({}));

        // The state published by the current connection is intact…
        let reg = state.registry.lock().expect("lock registry");
        let entry = reg.devices.get("d_test").expect("device present");
        assert_eq!(
            entry.relay_url.as_deref(),
            Some("https://relay-fresh.example/"),
            "relay_url overwritten by a replaced connection"
        );
        assert_eq!(entry.status.as_deref(), Some("ready"));
        // … and nothing was broadcast (the current connection — the only other
        // connection on the account — received no `device.updated`).
        assert!(
            current_rx.try_recv().is_err(),
            "unexpected broadcast for an ignored update"
        );
    }
}
