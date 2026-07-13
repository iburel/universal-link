// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! An IPC connection: lifecycle, method dispatch, enrollment.
//!
//! **Reads and writes are separated** — the same invariant as the server
//! (`server/src/conn.rs`): the main loop only reads, and every write goes
//! through a bounded queue drained by a dedicated task. A component that stops
//! reading freezes nothing and accumulates nothing; the FIFO queue guarantees
//! reply-before-close (deny, self-revocation).
//!
//! The connection's phase (`Fresh` / `Pending` / `Active`) lives in the
//! registry, not in the task: approval arrives over the GUI's connection, which
//! must be able to activate the requester's.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::rpc::{self, RpcErr};
use crate::state::{
    Active, AppState, ConnId, Enrolled, OutMsg, PendingRequest, Phase, enrich_device, random_hex,
};
use crate::transport::PeerInfo;
use crate::{API_VERSION, framing};

/// Depth of the write queue. Beyond it, the consumer is too slow: we disconnect
/// it rather than accumulate (it will resynchronize by reconnecting).
const OUT_QUEUE_DEPTH: usize = 256;
/// Beyond this, a write that makes no progress counts as a dead connection.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds for fields that are stored and later rebroadcast.
const NAME_MAX: usize = 128;
const VERSION_MAX: usize = 64;

const ROLES: [&str; 5] = ["gui", "clipboard-backend", "menu-backend", "tray", "custom"];
/// A single clipboard backend active at a time (doc/core-api.md, "Roles").
const EXCLUSIVE_ROLE: &str = "clipboard-backend";

const SCOPES: [&str; 9] = [
    "session.read",
    "session.manage",
    "devices.read",
    "devices.manage",
    "files.send",
    "transfers.read",
    "clipboard.read",
    "clipboard.write",
    "components.approve",
];

/// Never grantable through the approval prompt — only by the bootstrap trust
/// roots (otherwise: self-escalation).
const PROMPT_FORBIDDEN_SCOPE: &str = "components.approve";

fn topic_scope(topic: &str) -> Option<&'static str> {
    match topic {
        "session" => Some("session.read"),
        "devices" => Some("devices.read"),
        "transfers" => Some("transfers.read"),
        "clipboard" => Some("clipboard.read"),
        _ => None,
    }
}

struct Conn {
    state: Arc<AppState>,
    conn_id: ConnId,
    /// Sender toward its own queue (cloned into the registry so the other
    /// connections — approve, deny, revoke — can write to us).
    tx: mpsc::Sender<OutMsg>,
    peer: PeerInfo,
    /// A close aimed at this very connection: it must go out *after* the reply
    /// in flight (self-revocation), so the loop enqueues it itself.
    pending_close: bool,
}

pub async fn run(state: Arc<AppState>, stream: crate::transport::Stream, peer: PeerInfo) {
    let (tx, out_rx) = mpsc::channel(OUT_QUEUE_DEPTH);
    let conn_id = {
        let mut reg = state.registry.lock().expect("lock registry");
        // Accepted during Core shutdown (the drop's sweep ran before this
        // registration): we give up — otherwise the connection would survive,
        // served by the state of a dead Core.
        if reg.shutdown {
            return;
        }
        let id = reg.new_conn_id();
        reg.conns.insert(
            id,
            crate::state::ConnEntry {
                tx: tx.clone(),
                phase: Phase::Fresh,
            },
        );
        id
    };

    let (read, write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let mut writer = tokio::spawn(write_loop(write, out_rx));
    let mut conn = Conn {
        state: state.clone(),
        conn_id,
        tx,
        peer,
        pending_close: false,
    };
    let mut writer_done = false;

    loop {
        tokio::select! {
            // The write task stops on a requested close (deny, revoke —
            // including one decided by ANOTHER connection) or a dead peer.
            _ = &mut writer => {
                writer_done = true;
                break;
            }
            frame = framing::read_frame(&mut reader) => match frame {
                Ok(Some(text)) => {
                    if let Some(reply) = conn.handle_text(&text).await
                        && conn.tx.try_send(OutMsg::Frame(reply)).is_err()
                    {
                        break;
                    }
                    if conn.pending_close {
                        let _ = conn.tx.try_send(OutMsg::Close);
                        break;
                    }
                }
                // Clean EOF from the peer, or a framing violation: fail-closed,
                // no interpretable reply is owed.
                Ok(None) => break,
                Err(_) => {
                    let _ = conn.tx.try_send(OutMsg::Close);
                    break;
                }
            }
        }
    }

    conn.teardown();
    drop(conn);
    if !writer_done {
        // Let the queue (replies, enrollment.decided, close) drain before we
        // abandon it. Every sender has been removed: the task finishes on its
        // own once the queue is empty.
        let _ = tokio::time::timeout(WRITE_TIMEOUT, &mut writer).await;
        writer.abort();
    }
}

/// Drains the write queue. Any write that makes no progress within the allotted
/// time ends the task — the main loop detects it and exits.
async fn write_loop<W: AsyncWrite + Unpin>(mut sink: W, mut out_rx: mpsc::Receiver<OutMsg>) {
    while let Some(msg) = out_rx.recv().await {
        match msg {
            OutMsg::Frame(text) => {
                let bytes = framing::encode(&text);
                if !matches!(
                    tokio::time::timeout(WRITE_TIMEOUT, sink.write_all(&bytes)).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
            OutMsg::Close => break,
        }
    }
    // Unix: FIN → EOF at the peer. Windows: the pipe only closes when the whole
    // stream is destroyed — the main loop exits when this task finishes, and
    // drops both halves.
    let _ = sink.shutdown().await;
}

impl Conn {
    /// Handles a frame; returns the reply to send, if there is one. The `await`
    /// (proxy toward the server) serializes the requests of THIS connection —
    /// never a wait on the socket itself, so the write invariant holds.
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
        // Unknown client notification: never a reply (JSON-RPC 2.0).
        // (`clipboard.updated` and friends will arrive with their building
        // blocks.)
        let id = id.filter(|v| !v.is_null())?;

        Some(match self.dispatch(&method, &params).await {
            Ok(result) => rpc::response_ok(&id, result),
            Err(err) => rpc::response_err(&id, &err),
        })
    }

    async fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value, RpcErr> {
        if method == "hello" {
            return self.hello(params);
        }
        match method {
            "session.status" => {
                self.require_scope("session.read")?;
                Ok(self
                    .state
                    .session
                    .lock()
                    .expect("lock session")
                    .status_record())
            }
            "session.login" => self.session_login().await,
            "session.logout" => self.session_logout(),
            "account.status" => self.account_status(),
            "account.setup" => self.account_setup().await,
            "account.join" => self.account_join(params).await,
            "devices.list" => self.devices_list(),
            "devices.rename" => self.devices_rename(params).await,
            "devices.revoke" => self.devices_revoke(params).await,
            "files.send" => self.files_send(params),
            "files.cancel" => self.files_cancel(params),
            "events.subscribe" => self.events_subscribe(params),
            "components.list" => {
                self.require_scope("components.approve")?;
                Ok(self.components_list())
            }
            "components.pending" => {
                self.require_scope("components.approve")?;
                let reg = self.state.registry.lock().expect("lock registry");
                Ok(Value::Array(
                    reg.pending.values().map(PendingRequest::record).collect(),
                ))
            }
            "components.approve" => self.components_approve(params),
            "components.deny" => self.components_deny(params),
            "components.revoke" => self.components_revoke(params),
            _ => {
                // Phase first: an unenrolled component learns nothing about
                // the surface, not even which methods exist.
                self.require_enrolled()?;
                Err(RpcErr::method_not_found(method))
            }
        }
    }

    // -- Handshake ----------------------------------------------------------

    fn hello(&mut self, params: &Value) -> Result<Value, RpcErr> {
        let name = rpc::required_str_max(params, "name", NAME_MAX)?;
        let _version = rpc::required_str_max(params, "version", VERSION_MAX)?;
        let role = rpc::required_str(params, "role")?;
        let scopes = rpc::required_str_array(params, "scopes")?;
        let token = rpc::optional_str(params, "token")?;
        if !ROLES.contains(&role.as_str()) {
            return Err(RpcErr::invalid_params("role"));
        }
        if scopes.iter().any(|s| !SCOPES.contains(&s.as_str())) {
            return Err(RpcErr::invalid_params("scopes"));
        }

        let mut reg = self.state.registry.lock().expect("lock registry");
        match &reg
            .conns
            .get(&self.conn_id)
            .expect("live connection")
            .phase
        {
            Phase::Fresh => {}
            Phase::Pending(_) => return Err(RpcErr::app("PENDING_APPROVAL")),
            Phase::Active(_) => return Err(RpcErr::invalid_request()),
        }

        let Some(token) = token else {
            // Unknown third-party component: request queued, signaled to the
            // holders of the approval scope. No exclusivity check here: the
            // role conflict is a property of ACTIVATION — asking to replace the
            // backend in place is legitimate.
            let request_id = format!("r_{}", random_hex(8));
            let request = PendingRequest {
                request_id: request_id.clone(),
                conn_id: self.conn_id,
                name,
                role,
                scopes,
                peer_info: self.peer.record(),
            };
            let record = request.record();
            reg.pending.insert(request_id.clone(), request);
            reg.conns
                .get_mut(&self.conn_id)
                .expect("live connection")
                .phase = Phase::Pending(request_id);
            reg.notify_scope("components.approve", "component.pending", &record);
            return Ok(json!({ "status": "pending" }));
        };

        // Three families of tokens; in every case the role is bound to the
        // token (except the file token: full trust scope) and the requested
        // scopes are bounded by those of the grant.
        let component_id;
        if token == reg.file_token {
            component_id = format!("c_{}", random_hex(8));
        } else if let Some(grant) = reg.spawn_tokens.get(&token) {
            if grant.role != role {
                return Err(RpcErr::app("INVALID_TOKEN"));
            }
            if scopes.iter().any(|s| !grant.scopes.contains(s)) {
                return Err(RpcErr::app("SCOPE_DENIED"));
            }
            component_id = format!("c_{}", random_hex(8));
        } else if let Some(id) = reg.enrolled_tokens.get(&token) {
            let enrolled = reg.enrolled.get(id).expect("token index consistent");
            if enrolled.role != role {
                return Err(RpcErr::app("INVALID_TOKEN"));
            }
            if scopes.iter().any(|s| !enrolled.scopes.contains(s)) {
                return Err(RpcErr::app("SCOPE_DENIED"));
            }
            component_id = id.clone();
        } else {
            return Err(RpcErr::app("INVALID_TOKEN"));
        }

        if role == EXCLUSIVE_ROLE && reg.role_taken(EXCLUSIVE_ROLE) {
            return Err(RpcErr::app("ROLE_CONFLICT"));
        }
        // Hello accepted: it is now that a spawn token is consumed — a refused
        // hello (above) leaves the token and connection reusable.
        reg.spawn_tokens.remove(&token);
        if let Some(id) = reg.enrolled_tokens.get(&token).cloned() {
            let enrolled = reg.enrolled.get_mut(&id).expect("consistent index");
            enrolled.name = name.clone();
        }
        reg.conns
            .get_mut(&self.conn_id)
            .expect("live connection")
            .phase = Phase::Active(Active {
            component_id,
            name,
            role,
            scopes: scopes.clone(),
            topics: Vec::new(),
        });
        Ok(json!({
            "status": "ok",
            "granted_scopes": scopes,
            "api_version": API_VERSION,
        }))
    }

    // -- Server session and directory ---------------------------------------

    /// Starts the OIDC flow (PKCE + loopback) and returns the authorization URL
    /// — it is the caller that opens the browser. Completion will arrive via
    /// `session.changed`. A flow already pending is replaced.
    async fn session_login(&mut self) -> Result<Value, RpcErr> {
        self.require_scope("session.manage")?;
        if self.state.session.lock().expect("lock session").logged_in {
            // A single session at a time: logging in again begins with logout.
            return Err(RpcErr::app("ALREADY_LOGGED_IN"));
        }
        let auth_url = crate::login::start_flow(&self.state, crate::login::Goal::Login).await?;
        Ok(json!({ "auth_url": auth_url }))
    }

    /// Revokes a device of the account. The server demands a fresh ID token:
    /// the keyring's refresh token obtains one without a browser; otherwise
    /// (missing, dead, or judged too old by the server), re-auth goes through
    /// the same flow as login — `{ status: "reauth_required" }`, and completion
    /// is read from `device.removed`.
    async fn devices_revoke(&mut self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("devices.manage")?;
        let device_id = rpc::required_str(params, "device_id")?;
        // With no server connection, neither the proxy nor re-auth would
        // succeed: no point spending the refresh token.
        if self
            .state
            .session
            .lock()
            .expect("lock session")
            .server_tx
            .is_none()
        {
            return Err(RpcErr::app("SERVER_UNREACHABLE"));
        }
        match crate::login::fresh_id_token(&self.state).await {
            crate::login::FreshToken::Token(id_token) => {
                let result = crate::session::proxy(
                    &self.state,
                    "devices.revoke",
                    json!({ "device_id": device_id, "id_token": id_token }),
                )
                .await;
                match result {
                    Ok(_) => Ok(json!({ "status": "done" })),
                    // Not fresh enough for the server's taste: the browser will
                    // settle it.
                    Err(err) if err.app.as_deref() == Some("OIDC_INVALID") => {
                        self.revoke_reauth(device_id).await
                    }
                    Err(err) => Err(err),
                }
            }
            crate::login::FreshToken::NeedsReauth => self.revoke_reauth(device_id).await,
            crate::login::FreshToken::Unreachable => Err(RpcErr::app("SERVER_UNREACHABLE")),
        }
    }

    async fn revoke_reauth(&self, device_id: String) -> Result<Value, RpcErr> {
        let auth_url =
            crate::login::start_flow(&self.state, crate::login::Goal::Revoke { device_id }).await?;
        Ok(json!({ "status": "reauth_required", "auth_url": auth_url }))
    }

    /// Idempotent: outside a session there is nothing to close and nothing to
    /// say. Otherwise: session task stopped (the server sees the connection
    /// drop → offline), session.json removed, a single notification.
    fn session_logout(&mut self) -> Result<Value, RpcErr> {
        self.require_scope("session.manage")?;
        let abort = {
            let mut s = self.state.session.lock().expect("lock session");
            if !s.logged_in {
                return Ok(json!({}));
            }
            let (payload, abort) = s.forget();
            crate::session::remove_session_file(&self.state.config_dir);
            // The refresh token belonged to this session: it leaves with it.
            self.state.secrets.delete(crate::secrets::REFRESH_TOKEN);
            // Broadcast under the session lock (order: session then registry):
            // the order of notifications is the order of transitions — the
            // session task cannot slip a stale "connected" in after this
            // logout.
            self.state
                .registry
                .lock()
                .expect("lock registry")
                .notify_topic("session", "session.changed", &payload);
            abort
        };
        if let Some(abort) = abort {
            abort.abort();
        }
        // A pending re-auth flow belonged to the session: it dies with it.
        // (A LOGIN flow only exists outside a session — left untouched.)
        if let Some(slot) = self.state.login.lock().expect("lock login").take() {
            slot.abort.abort();
        }
        Ok(json!({}))
    }

    // -- Account key (C7) ---------------------------------------------------

    /// The state of the account's trust root: has this device joined the
    /// account, and under which fingerprint (safety number) — the anchor of
    /// out-of-band verification, to compare across devices.
    fn account_status(&self) -> Result<Value, RpcErr> {
        self.require_scope("session.read")?;
        let root = self.state.account_root.lock().expect("lock account_root");
        let fingerprint = root
            .as_ref()
            .and_then(|r| crate::account_key::fingerprint(&r.ak_pub));
        Ok(json!({ "attested": root.is_some(), "fingerprint": fingerprint }))
    }

    /// Creates the account key (first device): generates the recovery code,
    /// derives AK, attests OUR `node_id`, persists the root (AK_pub +
    /// attestation; AK_priv discarded) and publishes it. Returns the code — the
    /// ONLY copy of AK_priv, to hand to the user — and the fingerprint to
    /// compare on the other devices.
    async fn account_setup(&mut self) -> Result<Value, RpcErr> {
        self.require_scope("session.manage")?;
        self.require_server_connected()?;
        let code = crate::account_key::generate_recovery_code();
        let ak = crate::account_key::account_key_from_code(&code)
            .expect("freshly generated code is valid");
        let root = self.install_account_root(&ak)?;
        let fingerprint = crate::account_key::fingerprint(&root.ak_pub);
        self.publish_attestation(&root).await;
        Ok(json!({ "recovery_code": code, "fingerprint": fingerprint }))
    }

    /// Joins an existing account: re-derives AK from the entered code, attests
    /// OUR `node_id`, persists and publishes. Returns the fingerprint — to
    /// compare with that of the other devices (a divergence betrays a wrong
    /// code: this device would then stay outside the account, fail-closed).
    async fn account_join(&mut self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("session.manage")?;
        self.require_server_connected()?;
        let code = rpc::required_str(params, "recovery_code")?;
        let ak = crate::account_key::account_key_from_code(&code)
            .map_err(|_| RpcErr::app("INVALID_CODE"))?;
        let root = self.install_account_root(&ak)?;
        let fingerprint = crate::account_key::fingerprint(&root.ak_pub);
        self.publish_attestation(&root).await;
        Ok(json!({ "fingerprint": fingerprint }))
    }

    /// Attests our `node_id` under `ak`, persists the root and installs it in
    /// memory — atomically under the lock. Refuses if a root already exists:
    /// replacing it (AK rotation) is a follow-up building block; to start over,
    /// `account-key.json` must first be erased.
    fn install_account_root(
        &self,
        ak: &ed25519_dalek::SigningKey,
    ) -> Result<crate::account_key::AccountRoot, RpcErr> {
        let root = crate::account_key::root_for(ak, &self.state.identity.node_id());
        let mut slot = self.state.account_root.lock().expect("lock account_root");
        if slot.is_some() {
            return Err(RpcErr::app("ACCOUNT_KEY_SET"));
        }
        crate::account_key::save(&self.state.config_dir, &root)
            .map_err(|_| RpcErr::app("ACCOUNT_KEY_SAVE_FAILED"))?;
        *slot = Some(root.clone());
        Ok(root)
    }

    /// Refuses when the server is not connected: publishing the attestation
    /// would not succeed, and session setup has already decided what to publish
    /// — the device would stay unreachable for the whole session. Like
    /// `devices.revoke`, an account operation assumes the server is reachable
    /// (and the user just logged in to get here).
    fn require_server_connected(&self) -> Result<(), RpcErr> {
        if self
            .state
            .session
            .lock()
            .expect("lock session")
            .server_tx
            .is_none()
        {
            return Err(RpcErr::app("SERVER_UNREACHABLE"));
        }
        Ok(())
    }

    /// Publishes our attestation to the server then, on success, carries it
    /// onto OUR OWN cache record: the server excludes the publisher from its
    /// broadcast, and without this gesture our local directory would ignore
    /// itself until reconnection (same reasons as `set_own_relay` for the
    /// relay).
    async fn publish_attestation(&self, root: &crate::account_key::AccountRoot) {
        let published = crate::session::proxy(
            &self.state,
            "presence.update",
            json!({ "attestation": root.attestation }),
        )
        .await
        .is_ok();
        if published {
            crate::session::set_own_attestation(&self.state, &root.attestation);
        }
    }

    /// Serves the last known directory snapshot — even disconnected; freshness
    /// is read from `session.changed`. Without a snapshot since startup, there
    /// is nothing honest to serve: `SERVER_UNREACHABLE`.
    fn devices_list(&self) -> Result<Value, RpcErr> {
        self.require_scope("devices.read")?;
        let s = self.state.session.lock().expect("lock session");
        let Some(devices) = &s.devices else {
            return Err(RpcErr::app("SERVER_UNREACHABLE"));
        };
        let own = s.own_device_id.as_deref();
        Ok(Value::Array(
            devices.values().map(|d| enrich_device(d, own)).collect(),
        ))
    }

    /// Proxy toward the server. It is the session task that applies the reply
    /// to the cache and relays it as `device.updated` (in the order of the
    /// server flow, and even if we timed out in the meantime); here we only
    /// enrich the record for the reply to the component.
    async fn devices_rename(&mut self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("devices.manage")?;
        let device_id = rpc::required_str(params, "device_id")?;
        let name = rpc::required_str(params, "name")?;

        let result = crate::session::proxy(
            &self.state,
            "devices.rename",
            json!({ "device_id": device_id, "name": name }),
        )
        .await?;

        let enriched = {
            let s = self.state.session.lock().expect("lock session");
            let record = result.get("device").cloned().unwrap_or(Value::Null);
            enrich_device(&record, s.own_device_id.as_deref())
        };
        Ok(json!({ "device": enriched }))
    }

    // -- Transfers (T2) -----------------------------------------------------

    /// Starts an outgoing transfer toward `device_id`. Fire-and-forget: returns
    /// the `transfer_id` right away, tracking goes through the `transfers`
    /// topic. Resolving the peer verifies the C7 attestation (fail-closed).
    /// v1 "flat files": a directory in `paths` is refused (`-32602`).
    fn files_send(&self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("files.send")?;
        let device_id = rpc::required_str(params, "device_id")?;
        let paths = rpc::required_str_array(params, "paths")?;
        match crate::dataplane::start_send(&self.state, &device_id, &paths) {
            Ok(transfer_id) => Ok(json!({ "transfer_id": transfer_id })),
            Err(crate::dataplane::SendError::UnknownDevice) => Err(RpcErr::app("DEVICE_UNKNOWN")),
            Err(crate::dataplane::SendError::Offline) => Err(RpcErr::app("DEVICE_OFFLINE")),
            Err(crate::dataplane::SendError::BadPath(msg)) => Err(RpcErr::invalid_params(&msg)),
        }
    }

    /// Cancels a transfer (outgoing OR incoming) — it is its task that cleans
    /// up and emits the terminal outcome. `TRANSFER_UNKNOWN` if the id is
    /// unknown (already finished, or never existed).
    fn files_cancel(&self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("files.send")?;
        let transfer_id = rpc::required_str(params, "transfer_id")?;
        if crate::dataplane::cancel(&self.state, &transfer_id) {
            Ok(json!({}))
        } else {
            Err(RpcErr::app("TRANSFER_UNKNOWN"))
        }
    }

    // -- Methods ------------------------------------------------------------

    fn events_subscribe(&mut self, params: &Value) -> Result<Value, RpcErr> {
        // Phase before params: an unenrolled connection learns nothing about
        // the surface, not even the shape of the parameters.
        let scopes = self.require_enrolled()?;
        let topics = rpc::required_str_array(params, "topics")?;
        for topic in &topics {
            let scope = topic_scope(topic).ok_or_else(|| RpcErr::invalid_params("topics"))?;
            // All or nothing: no silent partial subscription.
            if !scopes.iter().any(|s| s == scope) {
                return Err(RpcErr::app("SCOPE_DENIED"));
            }
        }
        let mut reg = self.state.registry.lock().expect("lock registry");
        if let Phase::Active(active) = &mut reg
            .conns
            .get_mut(&self.conn_id)
            .expect("live connection")
            .phase
        {
            active.topics = topics;
        }
        Ok(json!({}))
    }

    fn components_list(&self) -> Value {
        let reg = self.state.registry.lock().expect("lock registry");
        let mut out = Vec::new();
        // The enrolled third parties, connected or not: enrollment survives the
        // connection, so the inventory must show it.
        for e in reg.enrolled.values() {
            let connected = reg
                .conns
                .values()
                .any(|c| matches!(&c.phase, Phase::Active(a) if a.component_id == e.component_id));
            out.push(json!({
                "component_id": e.component_id,
                "name": e.name,
                "role": e.role,
                "scopes": e.scopes,
                "connected": connected,
                "enrolled": true,
            }));
        }
        // The active bootstrap connections (spawned officials, GUI).
        // `enrolled: false`: there is no persistent token to revoke for them —
        // `components.revoke` would only close their connection. The role does
        // not let us recognize them (an approved third party may carry any
        // role), hence this field.
        for c in reg.conns.values() {
            if let Phase::Active(a) = &c.phase
                && !reg.enrolled.contains_key(&a.component_id)
            {
                out.push(json!({
                    "component_id": a.component_id,
                    "name": a.name,
                    "role": a.role,
                    "scopes": a.scopes,
                    "connected": true,
                    "enrolled": false,
                }));
            }
        }
        Value::Array(out)
    }

    fn components_approve(&mut self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("components.approve")?;
        let request_id = rpc::required_str(params, "request_id")?;
        let scopes = rpc::required_str_array(params, "scopes")?;
        if scopes.iter().any(|s| s == PROMPT_FORBIDDEN_SCOPE) {
            return Err(RpcErr::invalid_params("scopes"));
        }

        let mut reg = self.state.registry.lock().expect("lock registry");
        let Some(request) = reg.pending.get(&request_id) else {
            return Err(RpcErr::invalid_params("request_id"));
        };
        // Granted ⊆ requested: the prompt narrows, it never widens.
        if scopes.iter().any(|s| !request.scopes.contains(s)) {
            return Err(RpcErr::invalid_params("scopes"));
        }
        if request.role == EXCLUSIVE_ROLE && reg.role_taken(EXCLUSIVE_ROLE) {
            // The request survives: approvable once the incumbent has left.
            return Err(RpcErr::app("ROLE_CONFLICT"));
        }

        let request = reg.pending.remove(&request_id).expect("checked present");
        let token = random_hex(32);
        let component_id = format!("c_{}", random_hex(8));
        reg.enrolled_tokens
            .insert(token.clone(), component_id.clone());
        reg.enrolled.insert(
            component_id.clone(),
            Enrolled {
                component_id: component_id.clone(),
                token: token.clone(),
                name: request.name.clone(),
                role: request.role.clone(),
                scopes: scopes.clone(),
            },
        );
        // The requester's connection becomes active, bounded to the granted
        // scopes. (It exists: its teardown removes the request under this same
        // lock.)
        reg.conns
            .get_mut(&request.conn_id)
            .expect("queued request → live connection")
            .phase = Phase::Active(Active {
            component_id,
            name: request.name,
            role: request.role,
            scopes: scopes.clone(),
            topics: Vec::new(),
        });
        reg.notify_conn(
            request.conn_id,
            "enrollment.decided",
            &json!({ "approved": true, "token": token, "granted_scopes": scopes }),
        );
        Ok(json!({}))
    }

    fn components_deny(&mut self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("components.approve")?;
        let request_id = rpc::required_str(params, "request_id")?;

        let mut reg = self.state.registry.lock().expect("lock registry");
        let Some(request) = reg.pending.remove(&request_id) else {
            return Err(RpcErr::invalid_params("request_id"));
        };
        // The decision goes out before the close (FIFO queue).
        reg.notify_conn(
            request.conn_id,
            "enrollment.decided",
            &json!({ "approved": false }),
        );
        reg.close_conn(request.conn_id);
        Ok(json!({}))
    }

    fn components_revoke(&mut self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("components.approve")?;
        let component_id = rpc::required_str(params, "component_id")?;

        let mut reg = self.state.registry.lock().expect("lock registry");
        let mut known = false;
        if let Some(enrolled) = reg.enrolled.remove(&component_id) {
            reg.enrolled_tokens.remove(&enrolled.token);
            known = true;
        }
        let connected: Vec<ConnId> = reg
            .conns
            .iter()
            .filter(|(_, c)| matches!(&c.phase, Phase::Active(a) if a.component_id == component_id))
            .map(|(id, _)| *id)
            .collect();
        for conn_id in connected {
            known = true;
            if conn_id == self.conn_id {
                // Self-revocation: the close must follow the reply.
                self.pending_close = true;
            } else {
                reg.close_conn(conn_id);
            }
        }
        if !known {
            return Err(RpcErr::invalid_params("component_id"));
        }
        Ok(json!({}))
    }

    // -- Guardrails ---------------------------------------------------------

    /// The connection is active → its scopes; otherwise the phase error.
    fn require_enrolled(&self) -> Result<Vec<String>, RpcErr> {
        let reg = self.state.registry.lock().expect("lock registry");
        match &reg
            .conns
            .get(&self.conn_id)
            .expect("live connection")
            .phase
        {
            Phase::Fresh => Err(RpcErr::app("NOT_ENROLLED")),
            Phase::Pending(_) => Err(RpcErr::app("PENDING_APPROVAL")),
            Phase::Active(a) => Ok(a.scopes.clone()),
        }
    }

    fn require_scope(&self, scope: &str) -> Result<(), RpcErr> {
        if self.require_enrolled()?.iter().any(|s| s == scope) {
            Ok(())
        } else {
            Err(RpcErr::app("SCOPE_DENIED"))
        }
    }

    /// Removes the connection from the registry, and its pending enrollment
    /// request if any (a requester that has left has nothing left to approve).
    fn teardown(&mut self) {
        let mut reg = self.state.registry.lock().expect("lock registry");
        if let Some(entry) = reg.conns.remove(&self.conn_id)
            && let Phase::Pending(request_id) = entry.phase
        {
            reg.pending.remove(&request_id);
        }
    }
}
