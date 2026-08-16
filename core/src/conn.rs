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

const SCOPES: [&str; 11] = [
    "session.read",
    "session.manage",
    "devices.read",
    "devices.manage",
    "files.send",
    "transfers.read",
    "clipboard.read",
    "clipboard.write",
    "transactions.publish",
    "components.approve",
    "system.shutdown",
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
        // The only topic gated on a `manage` scope, deliberately: a pairing's
        // events drive a confirmation screen and carry the device asking to
        // join. Whoever watches one is whoever may answer it.
        "pairing" => Some("session.manage"),
        _ => None,
    }
}

/// Parses `transactions.fill`'s `entries`: a non-empty array of
/// `{ file_id, dest_path }` into pairs of strings; `-32602` otherwise.
fn parse_fill_entries(params: &Value) -> Result<Vec<(String, String)>, RpcErr> {
    let items = params
        .get("entries")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| RpcErr::invalid_params("entries"))?;
    items
        .iter()
        .map(|e| {
            let file_id = e
                .get("file_id")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcErr::invalid_params("file_id"))?;
            let dest_path = e
                .get("dest_path")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcErr::invalid_params("dest_path"))?;
            Ok((file_id.to_string(), dest_path.to_string()))
        })
        .collect()
}

/// Best-effort millisecond wall-clock: the ordering base for the clipboard's
/// global last-copier-wins. A clock skew only shifts the tie-break; local
/// monotonicity is guaranteed by the floor in `announce_local`.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
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
    let (read, write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);

    // The first frame routes the connection. An attach frame (a lone
    // `{ "channel_token": "…" }`, no `method`) turns it into a data channel;
    // anything else is the control plane. Reading it here keeps the two kinds
    // out of each other's way — a data channel is never registered as a control
    // connection, and carries no `hello`.
    let first = match framing::read_frame(&mut reader).await {
        Ok(Some(text)) => text,
        // Clean EOF or a framing violation before anything was said: nothing owed.
        Ok(None) | Err(_) => return,
    };
    if let Some(token) = channel_attach(&first) {
        crate::datachannel::run(state, reader, write, peer, token).await;
        return;
    }

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
                pid: peer.pid,
                pending: std::collections::HashMap::new(),
            },
        );
        id
    };

    let mut writer = tokio::spawn(write_loop(write, out_rx));
    let mut conn = Conn {
        state: state.clone(),
        conn_id,
        tx,
        peer,
        pending_close: false,
    };
    let mut writer_done = false;

    // Process the first frame (already read), then loop over the rest.
    if !conn.feed(&first).await {
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
                        if conn.feed(&text).await {
                            break;
                        }
                    }
                    // Clean EOF from the peer, or a framing violation:
                    // fail-closed, no interpretable reply is owed.
                    Ok(None) => break,
                    Err(_) => {
                        let _ = conn.tx.try_send(OutMsg::Close);
                        break;
                    }
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

/// A data-channel attach carries a `channel_token` and no `method`; the control
/// plane's first frame is always a `hello` (a `method`). Returns the token when
/// the frame is an attach.
fn channel_attach(first: &str) -> Option<String> {
    let v: Value = serde_json::from_str(first).ok()?;
    if v.get("method").is_some() {
        return None;
    }
    v.get("channel_token")
        .and_then(Value::as_str)
        .map(str::to_string)
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
    /// Processes one control-plane frame: dispatches it, enqueues the reply, and
    /// honors a self-revocation close. Returns `true` when the loop must end
    /// (the queue is dead, or a self-close is pending).
    async fn feed(&mut self, text: &str) -> bool {
        if let Some(reply) = self.handle_text(text).await
            && self.tx.try_send(OutMsg::Frame(reply)).is_err()
        {
            return true;
        }
        if self.pending_close {
            let _ = self.tx.try_send(OutMsg::Close);
            return true;
        }
        false
    }

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
            // No method: a RESPONSE to a request the Core issued to this
            // component (`clipboard.get_data`) carries `result` or `error` —
            // delivered to its waiter, no reply owed. Anything else with no
            // method is a malformed request (`-32600`, echoing the id).
            if msg.get("result").is_some() || msg.get("error").is_some() {
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    let outcome = match msg.get("error") {
                        Some(err) => Err(RpcErr::from_value(err)),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    self.state
                        .registry
                        .lock()
                        .expect("lock registry")
                        .deliver_response(self.conn_id, id, outcome);
                }
                return None;
            }
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
                Ok(self.session_status_result())
            }
            "session.login" => self.session_login().await,
            "session.logout" => self.session_logout(),
            "session.reload" => self.session_reload(),
            "session.discover" => self.session_discover(params).await,
            "account.status" => self.account_status(),
            "account.setup" => self.account_setup().await,
            "account.join" => self.account_join(params).await,
            "pairing.offer" => {
                self.require_scope("session.manage")?;
                crate::pairing::offer(&self.state).await
            }
            "pairing.accept" => {
                self.require_scope("session.manage")?;
                crate::pairing::accept(&self.state, &rpc::required_str(params, "code")?).await
            }
            "pairing.confirm" => {
                self.require_scope("session.manage")?;
                crate::pairing::confirm(&self.state, &rpc::required_str(params, "pairing_id")?)
                    .await
            }
            "pairing.cancel" => {
                self.require_scope("session.manage")?;
                crate::pairing::cancel(&self.state, &rpc::required_str(params, "pairing_id")?).await
            }
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
            "clipboard.updated" => self.clipboard_updated(params),
            "clipboard.current" => self.clipboard_current(),
            "transactions.open" => self.transactions_open(params),
            "transactions.fill" => self.transactions_fill(params),
            "transactions.publish" => self.transactions_publish(params),
            "transactions.revoke" => self.transactions_revoke(params),
            "transactions.adopt" => self.transactions_adopt(params).await,
            "system.shutdown" => self.system_shutdown(),
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
        match &reg.conns.get(&self.conn_id).expect("live connection").phase {
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

    /// `session.status` plus a `configured` flag — whether a server is set.
    /// Without it the GUI cannot tell "never configured" (→ show the first-run
    /// setup screen) from "configured but the server is down" (→ a transient
    /// error): both otherwise read as `server_connected: false`.
    fn session_status_result(&self) -> Value {
        let configured = self
            .state
            .server_config
            .lock()
            .expect("lock server_config")
            .is_some();
        let problem = self
            .state
            .config_problem
            .lock()
            .expect("lock config_problem")
            .clone();
        let mut v = self
            .state
            .session
            .lock()
            .expect("lock session")
            .status_record();
        v["configured"] = json!(configured);
        // The config file's health, cure sentence included (`null` when sound):
        // the one channel through which a faulty setting that fell back to its
        // default is ever confessed to the user.
        v["problem"] = json!(problem);
        v
    }

    /// Re-reads `config.json` (which the GUI has just written) and swaps the
    /// server config in place — no restart. The daemon-never-writes invariant
    /// holds: the Core only READS the file. A malformed / half-filled config is
    /// reported (`INVALID_CONFIG`, message = the reason) rather than silently
    /// leaving the Core unconfigured. Returns the fresh `session.status`.
    ///
    /// Meaningful outside a session: a live session stays pinned to the server
    /// it enrolled on (`SessionInfo.server_url`), so changing servers means
    /// logging out first — the GUI enforces that.
    /// Reads the deployment descriptor at an address the user typed and returns
    /// the settings to write into `config.json` — the interface then applies them
    /// with `session.reload`, the pair being what replaces a screen asking for
    /// the issuer and the client. Writes nothing itself: the Core is not the
    /// owner of that file.
    ///
    /// Meaningful precisely when nothing is configured, which is why it takes the
    /// address as a parameter instead of reading it from the config.
    ///
    /// It does make the Core fetch a URL a caller chose. That is bounded by the
    /// `session.manage` scope and by `http`'s own budget, and the failures name
    /// only the field at fault — never what came back (`discover::parse`), so it
    /// is not a way to read this machine's network through us.
    async fn session_discover(&mut self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("session.manage")?;
        let url = rpc::required_str(params, "url")?;
        match crate::discover::discover(self.state.connector.as_ref(), &url).await {
            Ok(deployment) => Ok(json!({
                "server_url": deployment.server_url,
                "oidc_issuer": deployment.oidc_issuer,
                "oidc_client_id": deployment.oidc_client_id,
                "oidc_client_secret": deployment.oidc_client_secret,
            })),
            Err(crate::discover::DiscoverError::BadUrl) => Err(RpcErr::invalid_params("url")),
            Err(crate::discover::DiscoverError::NoDescriptor) => Err(RpcErr::app_message(
                "NO_DESCRIPTOR",
                "this server does not publish its settings",
            )),
            Err(crate::discover::DiscoverError::Unreachable(why)) => {
                Err(RpcErr::app_message("SERVER_UNREACHABLE", why))
            }
            Err(crate::discover::DiscoverError::Invalid(why)) => {
                Err(RpcErr::app_message("INVALID_DESCRIPTOR", why))
            }
        }
    }

    fn session_reload(&self) -> Result<Value, RpcErr> {
        self.require_scope("session.manage")?;
        // Both arms refresh `config_problem`: `session.status` mirrors the
        // LAST parse of the file. A failed reload keeps the running server
        // config (the fresh file earned nothing) but its reason is still what
        // the interface must show from now on; a sound reload clears a stale
        // boot-time reason along with applying the config.
        match (self.state.reload_server)() {
            Ok(server) => {
                *self.state.server_config.lock().expect("lock server_config") = server;
                *self
                    .state
                    .config_problem
                    .lock()
                    .expect("lock config_problem") = None;
                Ok(self.session_status_result())
            }
            Err(reason) => {
                *self
                    .state
                    .config_problem
                    .lock()
                    .expect("lock config_problem") = Some(reason.clone());
                Err(RpcErr::app_message("INVALID_CONFIG", reason))
            }
        }
    }

    /// Revokes a device of the account. The server demands a fresh ID token:
    /// the keyring's refresh token obtains one without a browser; otherwise
    /// (missing, dead, or judged too old by the server), re-auth goes through
    /// the same flow as login — `{ status: "reauth_required" }`, and completion
    /// is read from `device.removed`.
    async fn devices_revoke(&mut self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("devices.manage")?;
        let device_id = rpc::required_str(params, "device_id")?;
        if self.serverless() {
            return self.revoke_locally(&device_id);
        }
        // The target's crypto identity, read while its record is still here (the
        // eviction below would erase it): it is what the tombstone names.
        let target_node_id = {
            let s = self.state.session.lock().expect("lock session");
            s.devices
                .as_ref()
                .and_then(|devices| devices.get(&device_id))
                .and_then(|record| record.get("node_id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        // A device the server never named — its label IS its `node_id`, the
        // self-minted convention — was learned from the account's serverless
        // half, and asking the server to strike it would only earn a
        // DEVICE_UNKNOWN: the account's signature is the one authority over it.
        if target_node_id.as_deref() == Some(device_id.as_str()) {
            return self.revoke_locally(&device_id);
        }
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
                    Ok(_) => {
                        // The server struck it off ITS directory; where this
                        // device holds the account key, the account says so too —
                        // a tombstone, so the serverless half hears the
                        // revocation a server-side strike alone would never
                        // reach (the continuum).
                        if let Some(node_id) = &target_node_id {
                            crate::account_key::tombstone_after_server_revoke(&self.state, node_id);
                        }
                        Ok(json!({ "status": "done" }))
                    }
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

    /// Revokes a device with no server in the picture: the ACCOUNT KEY signs a
    /// tombstone against its `node_id` (`account_key::revoke`), the record leaves
    /// the directory, and the tombstone stays — so nothing brings it back, here or
    /// on any device this one later syncs with. This is what replaces "strike it
    /// from the server directory" where there is no directory to strike it from.
    ///
    /// Three things it is honest to state plainly. It is **permanent**: nothing
    /// can un-revoke a `node_id`, and a device struck off by mistake comes back
    /// only as a new one (a fresh `device.key`, attested again). It needs the
    /// account's PRIVATE key — the account is what strikes a device off, not this
    /// machine — so a device that holds the account without its key cannot do it
    /// (`NO_ACCOUNT_KEY`, the same answer it gives when asked to vouch). And there
    /// is **no fresh-login gate**: with no server there is no OIDC to be fresh
    /// with, so what stands between a component and this call is the
    /// `devices.manage` scope, nothing more.
    fn revoke_locally(&self, device_id: &str) -> Result<Value, RpcErr> {
        // The keyring first: `recall` waits on an OS service (seconds, in the bad
        // case) and no state lock may be held across it.
        let ak_pub = self
            .state
            .account_root
            .lock()
            .expect("lock account_root")
            .as_ref()
            .map(|root| root.ak_pub.clone());
        let ak = ak_pub
            .as_deref()
            .and_then(|ak_pub| crate::account_key::recall(&*self.state.secrets, ak_pub))
            .ok_or_else(|| RpcErr::app("NO_ACCOUNT_KEY"))?;

        let mut s = self.state.session.lock().expect("lock session");
        let node_id = s
            .devices
            .as_ref()
            .and_then(|devices| devices.get(device_id))
            .and_then(|record| record.get("node_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RpcErr::app("DEVICE_UNKNOWN"))?;
        // Striking ourselves off would bar this installation from its own account
        // with no way back — every peer would refuse it, and the tombstone cannot
        // be withdrawn. Leaving the account is `session.logout` plus erasing the
        // trust root, not a signature against oneself.
        if node_id == self.state.identity.node_id() {
            return Err(RpcErr::app("CANNOT_REVOKE_SELF"));
        }
        let struck = s.strike_off(&node_id, crate::account_key::revoke(&ak, &node_id));
        // Disk before the notification, under the lock that mutated the state
        // (state-then-disk): a component told a device is gone can rely on a
        // restart still knowing it.
        crate::directory::save_revoked(&self.state.config_dir, &s.revoked);
        if let Some(devices) = s.devices.as_ref() {
            crate::directory::save(&self.state.config_dir, devices);
        }
        let registry = self.state.registry.lock().expect("lock registry");
        for id in struck {
            registry.notify_topic("devices", "device.removed", &json!({ "device_id": id }));
        }
        // The tombstone has to reach the account's other devices, and a struck-off
        // device is the last thing to leave waiting for the next tick.
        self.state.dirsync_wake.notify_one();
        Ok(json!({ "status": "done" }))
    }

    /// Idempotent: outside a session there is nothing to close and nothing to
    /// say. Otherwise: session task stopped (the server sees the connection
    /// drop → offline), session.json removed, a single notification.
    fn session_logout(&mut self) -> Result<Value, RpcErr> {
        self.require_scope("session.manage")?;
        // Read out of the leaf lock before taking `session` (lock ordering).
        // Logging out of a server does not leave the ACCOUNT: the trust root
        // stays (it is what a re-enrollment or a pairing rests on), so what this
        // device knows first-hand about itself has to survive the session too —
        // and, since the continuum, so does every record the account can still
        // prove under `ak_pub` (`forget`).
        let root = self
            .state
            .account_root
            .lock()
            .expect("lock account_root")
            .clone();
        let own = root.as_ref().map(|root| crate::state::OwnDevice {
            identity: &self.state.identity,
            name: &self.state.device_name,
            attestation: &root.attestation,
        });
        let abort = {
            let mut s = self.state.session.lock().expect("lock session");
            if !s.logged_in {
                return Ok(json!({}));
            }
            let (payload, abort) = s.forget(own, root.as_ref().map(|root| root.ak_pub.as_str()));
            crate::session::remove_session_file(&self.state.config_dir);
            // The relay announcement was the server's standing word, and the
            // standing ends with the session (#105). Records keep the hints
            // they signed while it stood; a stale one costs a failed dial.
            crate::relays::forget(&self.state);
            // What the SERVER asserted belonged to this session and leaves with
            // it; the account's half — the records `forget` kept — does not.
            // `save_unrefreshed`: a logout re-checks nothing against an authority.
            match s.devices.as_ref() {
                Some(devices) => {
                    crate::directory::save_unrefreshed(&self.state.config_dir, devices);
                }
                None => crate::directory::remove(&self.state.config_dir),
            }
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
        // Logging out drops every clipboard transaction and cuts the open
        // consumer channels (`TX_STALE`): the account's read grants do not
        // outlive its session. (Reached only on a real logout — the not-logged-in
        // case returned above.)
        self.state
            .clipboard
            .lock()
            .expect("lock clipboard")
            .clear_all();
        self.state.clipboard_reset.notify_waiters();
        Ok(json!({}))
    }

    // -- Account key (C7) ---------------------------------------------------

    /// The state of the account's trust root: has this device joined the
    /// account, and under which fingerprint (safety number) — the anchor of
    /// out-of-band verification, to compare across devices.
    fn account_status(&self) -> Result<Value, RpcErr> {
        self.require_scope("session.read")?;
        // Cloned out of the lock: the keyring read below can block (a mute
        // keyring is only given up on after a timeout), and `account_root` is on
        // the data plane's authorization path — holding it that long would stall
        // every peer check.
        let ak_pub = self
            .state
            .account_root
            .lock()
            .expect("lock account_root")
            .as_ref()
            .map(|r| r.ak_pub.clone());
        let fingerprint = ak_pub.as_deref().and_then(crate::account_key::fingerprint);
        // Read back rather than remembered from the write: on the desktop a
        // keyring write is queued, so only a read tells the truth about what the
        // device actually holds — and that is what decides whether it can vouch
        // for a joining device.
        let holds_key = ak_pub.as_deref().is_some_and(|pub_hex| {
            crate::account_key::recall(&*self.state.secrets, pub_hex).is_some()
        });
        Ok(json!({
            "attested": ak_pub.is_some(),
            "fingerprint": fingerprint,
            "holds_key": holds_key,
        }))
    }

    /// Creates the account key (first device): generates the recovery code,
    /// derives AK, attests OUR `node_id`, persists the root (AK_pub +
    /// attestation) plus AK_priv in the keyring, and publishes it. Returns the
    /// code — the user's way back if every device is lost — and the fingerprint
    /// to compare on the other devices.
    async fn account_setup(&mut self) -> Result<Value, RpcErr> {
        self.require_scope("session.manage")?;
        self.require_server_if_there_is_one()?;
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
    ///
    /// Entering the code of the account this device is ALREADY in is accepted and
    /// changes nothing but the keyring: it is the way back in for a device that
    /// has the account but not its key (see `install_account_root`).
    async fn account_join(&mut self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("session.manage")?;
        self.require_server_if_there_is_one()?;
        let code = rpc::required_str(params, "recovery_code")?;
        let ak = crate::account_key::account_key_from_code(&code)
            .map_err(|_| RpcErr::app("INVALID_CODE"))?;
        let root = self.install_account_root(&ak)?;
        let fingerprint = crate::account_key::fingerprint(&root.ak_pub);
        self.publish_attestation(&root).await;
        Ok(json!({ "fingerprint": fingerprint }))
    }

    /// Installs the trust root derived from a typed recovery code
    /// (`account_key::install` holds the rules, shared with the pairing path) and
    /// names its refusals in this API's vocabulary.
    fn install_account_root(
        &self,
        ak: &ed25519_dalek::SigningKey,
    ) -> Result<crate::account_key::AccountRoot, RpcErr> {
        crate::account_key::install(&self.state, ak).map_err(|e| match e {
            crate::account_key::InstallError::OtherKey => RpcErr::app("ACCOUNT_KEY_SET"),
            crate::account_key::InstallError::SaveFailed => RpcErr::app("ACCOUNT_KEY_SAVE_FAILED"),
        })
    }

    /// Is there **no server in this device's life at all**? The rules, and why
    /// getting them wrong the lenient way is the expensive mistake, are in
    /// [`crate::state::serverless`] — the pairing turns on the same answer, and two
    /// definitions of it would drift.
    fn serverless(&self) -> bool {
        crate::state::serverless(&self.state)
    }

    /// A server this device has must be connected: joining the account publishes
    /// an attestation, and session setup has already decided what to publish —
    /// the device would otherwise stay unreachable for the whole session. Like
    /// `devices.revoke`, an account operation on a deployment assumes the server
    /// is reachable (and the user just logged in to get here).
    ///
    /// With **no server at all** ([`Self::serverless`]) there is nothing to
    /// publish to, and nothing to be unreachable for: the trust root, the
    /// directory and this device's own record are all local. Refusing here would
    /// mean an account could never be created without a server, which is precisely
    /// what this path is for.
    fn require_server_if_there_is_one(&self) -> Result<(), RpcErr> {
        if self.serverless() {
            return Ok(());
        }
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

    async fn publish_attestation(&self, root: &crate::account_key::AccountRoot) {
        crate::session::publish_attestation(&self.state, root).await;
    }

    /// Serves the last known directory snapshot — even disconnected; freshness
    /// is read from `session.changed`. Without a snapshot since startup, there
    /// is nothing honest to serve: `SERVER_UNREACHABLE`.
    fn devices_list(&self) -> Result<Value, RpcErr> {
        self.require_scope("devices.read")?;
        // Transport snapshot BEFORE the session lock (lock ordering: the
        // transport has a lock of its own).
        let lan: std::collections::BTreeSet<String> =
            self.state.transport.lan_peers().into_iter().collect();
        let s = self.state.session.lock().expect("lock session");
        let Some(devices) = &s.devices else {
            return Err(RpcErr::app("SERVER_UNREACHABLE"));
        };
        let own = s.own_device_id.as_deref();
        Ok(Value::Array(
            devices
                .values()
                .map(|d| enrich_device(d, own, s.server_connected, &lan))
                .collect(),
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
        if self.serverless() {
            return self.rename_own(&device_id, &name);
        }

        let result = crate::session::proxy(
            &self.state,
            "devices.rename",
            json!({ "device_id": device_id, "name": name }),
        )
        .await?;

        let enriched = {
            let lan: std::collections::BTreeSet<String> =
                self.state.transport.lan_peers().into_iter().collect();
            let s = self.state.session.lock().expect("lock session");
            let record = result.get("device").cloned().unwrap_or(Value::Null);
            enrich_device(
                &record,
                s.own_device_id.as_deref(),
                s.server_connected,
                &lan,
            )
        };
        Ok(json!({ "device": enriched }))
    }

    /// Renames this device with no server in the picture. A device is the
    /// authority on its own description and on nothing else: it re-signs its
    /// record under a higher `seq`, which is what makes a peer that already knows
    /// it prefer the new name (`directory`).
    ///
    /// **Any other `device_id` answers `SERVER_UNREACHABLE`** — unchanged from
    /// before this path existed. That is not a stopgap: another device's record is
    /// signed by that device, so renaming it means asking it, and the only thing
    /// that carries such an ask is the server. A name that only our directory
    /// believed in would be worse than a refusal, because it would disagree with
    /// every other device in the account and nothing would ever settle it.
    fn rename_own(&self, device_id: &str, name: &str) -> Result<Value, RpcErr> {
        // Read out of the leaf locks before taking `session` (lock ordering).
        let attestation = self
            .state
            .account_root
            .lock()
            .expect("lock account_root")
            .as_ref()
            .map(|root| root.attestation.clone());
        let lan: std::collections::BTreeSet<String> =
            self.state.transport.lan_peers().into_iter().collect();
        let unreachable = || RpcErr::app("SERVER_UNREACHABLE");
        let attestation = attestation.ok_or_else(unreachable)?;
        let own = crate::state::OwnDevice {
            identity: &self.state.identity,
            name: &self.state.device_name,
            attestation: &attestation,
        };

        let mut s = self.state.session.lock().expect("lock session");
        if s.own_device_id.as_deref() != Some(device_id) {
            return Err(unreachable());
        }
        let record = s.rename_own(own, name).ok_or_else(unreachable)?;
        let enriched = enrich_device(
            &record,
            s.own_device_id.as_deref(),
            s.server_connected,
            &lan,
        );
        if let Some(devices) = s.devices.as_ref() {
            crate::directory::save(&self.state.config_dir, devices);
        }
        // Broadcast under the session lock (order: session then registry), like
        // every directory change: the order of notifications is the order of
        // states. And broadcast at all, unlike the server path — there the
        // server's reply is what fans out to the other components.
        self.state
            .registry
            .lock()
            .expect("lock registry")
            .notify_topic(
                "devices",
                "device.updated",
                &json!({ "device": enriched.clone() }),
            );
        // A name only this device knows is a name nobody uses: the new description
        // goes out to the account's other devices at once (`dirsync`).
        self.state.dirsync_wake.notify_one();
        Ok(json!({ "device": enriched }))
    }

    // -- Transfers (T2) -----------------------------------------------------

    /// Starts an outgoing transfer toward `device_id`. Fire-and-forget: returns
    /// the `transfer_id` right away, tracking goes through the `transfers`
    /// topic. Resolving the peer verifies the C7 attestation (fail-closed). A
    /// directory in `paths` is walked into a tree manifest (the same walk the
    /// clipboard uses); an unrepresentable name or an over-cap manifest is
    /// refused (`-32602` / `MANIFEST_TOO_LARGE`).
    fn files_send(&self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("files.send")?;
        let device_id = rpc::required_str(params, "device_id")?;
        let paths = rpc::required_str_array(params, "paths")?;
        match crate::dataplane::start_send(&self.state, &device_id, &paths) {
            Ok(transfer_id) => Ok(json!({ "transfer_id": transfer_id })),
            Err(crate::dataplane::SendError::UnknownDevice) => Err(RpcErr::app("DEVICE_UNKNOWN")),
            Err(crate::dataplane::SendError::Offline) => Err(RpcErr::app("DEVICE_OFFLINE")),
            Err(crate::dataplane::SendError::BadPath(msg)) => Err(RpcErr::invalid_params(&msg)),
            Err(crate::dataplane::SendError::Rejected(err)) => Err(err),
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

    // -- Clipboard (transactions) -------------------------------------------

    /// Announces a local copy: opens the transaction that supersedes the
    /// previous clip (last copier wins). Only metadata travels — for files, the
    /// manifest is frozen from `paths` (canonicalized + `stat`ed, no byte read).
    /// An empty `formats` means the clipboard was cleared (a contentless
    /// transaction, which supersedes like any other).
    fn clipboard_updated(&self, params: &Value) -> Result<Value, RpcErr> {
        // Announcing is the exclusive backend's privilege (role + scope), not a
        // right any `clipboard.write` holder gets.
        self.require_clipboard_backend()?;
        let mut formats = crate::clipboard::parse_formats(params)?;
        let sensitive = rpc::optional_bool(params, "sensitive")?.unwrap_or(false);
        let paths = rpc::optional_str_array(params, "paths")?;
        let has_files = formats.iter().any(|f| f.format == "files");
        // `paths` present iff a `files` format is offered — no silent mismatch.
        let files = match (has_files, paths) {
            (true, Some(paths)) if !paths.is_empty() => crate::clipboard::freeze_manifest(&paths)?,
            (false, None) => Vec::new(),
            _ => return Err(RpcErr::invalid_params("paths")),
        };
        // Materialized (push-at-copy, Model B): the caller supplies the inline
        // bytes now. `parse_materialized` validates (inline-only, non-sensitive,
        // capped) and rewrites each format's `size` to its exact decoded length.
        let materialized_blobs =
            crate::clipboard::parse_materialized(params, &mut formats, sensitive, has_files)?;
        let materialized: std::collections::HashMap<String, std::sync::Arc<Vec<u8>>> =
            materialized_blobs
                .as_ref()
                .map(|b| b.iter().cloned().collect())
                .unwrap_or_default();
        let device_id = self
            .state
            .session
            .lock()
            .expect("lock session")
            .own_device_id
            .clone();
        let tx = crate::clipboard::Transaction {
            tx_id: format!("tx_{}", random_hex(16)),
            device_id,
            seq: 0, // assigned by `announce_local` (floored above the current clip)
            formats,
            files,
            sensitive,
            producer: crate::clipboard::Producer::Clipboard,
            origin: crate::clipboard::Origin::Local {
                announcer: self.conn_id,
            },
            superseded: false,
            sessions: 0,
            materialized,
        };
        // Announce locally (last copier wins here), then broadcast to the
        // account's other devices so they converge on this copy: a plain
        // `clip_announce` (metadata only, pulled at paste), or — when
        // materialized — a `clip_push` that also carries the inline bytes.
        let (tx_id, net) = {
            let mut cb = self.state.clipboard.lock().expect("lock clipboard");
            let tx_id = cb.announce_local(tx, now_millis());
            let net = cb.network_announce_of(&tx_id);
            (tx_id, net)
        };
        // A materialized announce also reports its fan-out: `pushed_to` says how
        // many devices the push was launched to, and a non-zero count promises
        // exactly one `clipboard.pushed` on THIS connection once they settle. A
        // source that vanishes right after the copy has no other way to learn
        // whether its share landed — and zero devices must not read as success.
        let materialized = materialized_blobs.is_some();
        let mut pushed_to = 0;
        if let Some(net) = net {
            match materialized_blobs {
                Some(blobs) => {
                    pushed_to = crate::clipnet::propagate_materialized(
                        &self.state,
                        net,
                        blobs,
                        self.conn_id,
                        tx_id.clone(),
                    );
                }
                None => crate::clipnet::propagate(&self.state, net),
            }
        }
        if materialized {
            Ok(json!({ "tx_id": tx_id, "pushed_to": pushed_to }))
        } else {
            Ok(json!({ "tx_id": tx_id }))
        }
    }

    /// The `clipboard` topic's snapshot method: the current global clip (or `{}`
    /// if none). A backend that (re)connects re-learns the live promise here
    /// before subscribing — the resync rule of `events.subscribe`.
    fn clipboard_current(&self) -> Result<Value, RpcErr> {
        self.require_scope("clipboard.read")?;
        Ok(self
            .state
            .clipboard
            .lock()
            .expect("lock clipboard")
            .current_record())
    }

    /// Publishes a LONG-LIVED transaction over `paths` (doc/core-api.md,
    /// "Transactions"): the very manifest freeze and guards of a clipboard
    /// announce, but OUTSIDE the election - the current clip is untouched,
    /// nothing supersedes the published transaction, and it serves any number
    /// of sessions until `transactions.revoke`, until this connection closes (a
    /// grant dies with its holder), or until the account's grants die with the
    /// session (logout / Core stop). Guarded by the producer scope alone, no
    /// role: any component may be a producer.
    fn transactions_publish(&self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("transactions.publish")?;
        let paths = rpc::required_str_array(params, "paths")?;
        if paths.is_empty() {
            return Err(RpcErr::invalid_params("paths"));
        }
        let files = crate::clipboard::freeze_manifest(&paths)?;
        let device_id = self
            .state
            .session
            .lock()
            .expect("lock session")
            .own_device_id
            .clone();
        let tx = crate::clipboard::Transaction {
            tx_id: format!("tx_{}", random_hex(16)),
            device_id,
            seq: 0, // outside the election: never compared
            // Files-only by construction: a `FETCH` answers `FORMAT_UNKNOWN`,
            // and no inline pull ever reaches the publisher (which is not a
            // clipboard backend and answers no `clipboard.get_data`).
            formats: Vec::new(),
            files,
            sensitive: false,
            producer: crate::clipboard::Producer::Published {
                owner: self.conn_id,
            },
            origin: crate::clipboard::Origin::Local {
                announcer: self.conn_id,
            },
            superseded: false,
            sessions: 0,
            materialized: std::collections::HashMap::new(),
        };
        // The publisher gets the full frozen record back - relative wire paths
        // and Core-assigned `file_id`s, never the source paths: the manifest it
        // will speak to its peers about, exactly what an adopter installs.
        let record = {
            let mut cb = self.state.clipboard.lock().expect("lock clipboard");
            let tx_id = cb.publish(tx);
            cb.published_record(&tx_id).expect("just published")
        };
        Ok(record)
    }

    /// Revokes a published transaction: the right to read ends NOW - open
    /// channels are cut with `TX_STALE` (the same cutting path a logout takes)
    /// and new opens refuse. Idempotent on an unknown `tx_id` (a revoke retried
    /// after a timeout, or racing the publisher's own teardown, is not an
    /// error); a CLIPBOARD transaction is `-32602` - its lifecycle belongs to
    /// the election, not to a revoker.
    fn transactions_revoke(&self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("transactions.publish")?;
        let tx_id = rpc::required_str(params, "tx_id")?;
        let removed = self
            .state
            .clipboard
            .lock()
            .expect("lock clipboard")
            .revoke(&tx_id)?;
        if removed {
            self.state.clipboard_reset.notify_waiters();
        }
        Ok(json!({}))
    }

    /// Installs a transaction PUBLISHED on another device of the account, so
    /// the untouched consumer machinery (`transactions.open`, `transactions.fill`,
    /// the data channel) can then reach it: fetches the frozen manifest from
    /// the source (`tx_fetch` on the data plane), re-validates it fail-closed
    /// (exactly like a remote clip announce), and returns the installed record.
    /// The entry dies with this connection, like a published one - re-adopting
    /// after a reconnect is cheap and idempotent.
    async fn transactions_adopt(&self, params: &Value) -> Result<Value, RpcErr> {
        self.require_scope("transactions.publish")?;
        let device_id = rpc::required_str(params, "device_id")?;
        let tx_id = rpc::required_str(params, "tx_id")?;
        crate::clipnet::adopt(&self.state, self.conn_id, &device_id, &tx_id).await
    }

    /// Opens a consumer channel for `tx_id`: mints an unguessable `channel_token`
    /// bound to this component (peer credentials). The transaction must be
    /// openable — a superseded clip accepts no NEW session (`TX_STALE`). The
    /// session itself begins when the data channel attaches with the token.
    /// Consuming requires the scope of the transaction's PRODUCER
    /// (doc/core-api.md, "Transactions"): `clipboard.read` for a clipboard
    /// transaction, `transactions.publish` for a published one. A caller
    /// holding neither learns nothing (`SCOPE_DENIED` before any lookup).
    fn transactions_open(&self, params: &Value) -> Result<Value, RpcErr> {
        let scopes = self.require_enrolled()?;
        let has = |s: &str| scopes.iter().any(|x| x == s);
        if !has("clipboard.read") && !has("transactions.publish") {
            return Err(RpcErr::app("SCOPE_DENIED"));
        }
        let tx_id = rpc::required_str(params, "tx_id")?;
        let (origin, materialized) = {
            let cb = self.state.clipboard.lock().expect("lock clipboard");
            if !cb.is_openable(&tx_id) {
                return Err(RpcErr::app("TX_STALE"));
            }
            let needed = cb.consume_scope_of(&tx_id).expect("openable transaction");
            if !has(needed) {
                return Err(RpcErr::app("SCOPE_DENIED"));
            }
            (cb.origin_of(&tx_id), cb.is_materialized(&tx_id))
        };
        // A remote clip whose source is no longer reachable (re-enrolled under a
        // new node_id, or with no route to it — no published relay, not seen on
        // the LAN) fails fast here — the control-plane twin of the data
        // channel's `PEER_GONE`. A MATERIALIZED remote clip is exempt: it is
        // served from the local cache, so the source need not be reachable —
        // indeed it may already be gone, which is the whole point of
        // push-at-copy.
        if !materialized
            && let Some(crate::clipboard::Origin::Remote { node_id, device_id }) = origin
        {
            let reachable =
                crate::dataplane::resolve_peer(&self.state, &device_id).is_some_and(|p| {
                    p.node_id == node_id && crate::dataplane::peer_reachable(&self.state, &p)
                });
            if !reachable {
                return Err(RpcErr::app("DEVICE_OFFLINE"));
            }
        }
        let token = self
            .state
            .registry
            .lock()
            .expect("lock registry")
            .mint_channel_token(crate::state::ChannelGrant {
                tx_id,
                kind: crate::state::ChannelKind::Consumer,
                pid: self.peer.pid,
                conn_id: self.conn_id,
                sink: None,
            });
        Ok(json!({ "channel_token": token }))
    }

    /// Designates target files for the Core to fill from a transaction (the
    /// paste surface's skeleton paths). Fire-and-forget like `files.send`: the
    /// `transfer_id` comes back at once, progress and completion arrive over the
    /// `transfers` topic, cancellation via `files.cancel`. The `dest_path`s come
    /// from the enrolled backend (the `files.send` trust model — the remote
    /// manifest never chooses where bytes land); the Core creates their missing
    /// parents and writes them directly (an OS-watched skeleton admits no
    /// temp+rename). The `file_id`s must be non-`dir` manifest entries.
    fn transactions_fill(&self, params: &Value) -> Result<Value, RpcErr> {
        // Same per-producer consuming scope as `transactions.open`.
        let scopes = self.require_enrolled()?;
        let has = |s: &str| scopes.iter().any(|x| x == s);
        if !has("clipboard.read") && !has("transactions.publish") {
            return Err(RpcErr::app("SCOPE_DENIED"));
        }
        let tx_id = rpc::required_str(params, "tx_id")?;
        let entries = parse_fill_entries(params)?;
        let plan = {
            let cb = self.state.clipboard.lock().expect("lock clipboard");
            let needed = cb
                .consume_scope_of(&tx_id)
                .ok_or_else(|| RpcErr::app("TX_STALE"))?;
            if !has(needed) {
                return Err(RpcErr::app("SCOPE_DENIED"));
            }
            cb.fill_plan(&tx_id, &entries)?
        };
        let (transfer_id, cancel) = self
            .state
            .transfers
            .lock()
            .expect("lock transfers")
            .register();
        tokio::spawn(crate::clipnet::run_fill(
            self.state.clone(),
            transfer_id.clone(),
            tx_id,
            plan,
            cancel,
        ));
        Ok(json!({ "transfer_id": transfer_id }))
    }

    // -- Lifecycle ----------------------------------------------------------

    /// Stops the whole Core — the tray's Quit. The library only SIGNALS; the
    /// binary owns the orderly teardown and awaits this next to the OS signals.
    /// The reply leaves before that teardown reaches this connection: the binary
    /// stops the components first, ample time for this connection's queue to
    /// drain (the same reply-before-close guarantee as a self-revocation).
    /// Relaunching means opening the GUI, which respawns the Core.
    fn system_shutdown(&self) -> Result<Value, RpcErr> {
        self.require_scope("system.shutdown")?;
        self.state.shutdown_request.notify_one();
        Ok(json!({}))
    }

    // -- Guardrails ---------------------------------------------------------

    /// The connection is active → its scopes; otherwise the phase error.
    fn require_enrolled(&self) -> Result<Vec<String>, RpcErr> {
        let reg = self.state.registry.lock().expect("lock registry");
        match &reg.conns.get(&self.conn_id).expect("live connection").phase {
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

    /// Announcing (and answering `clipboard.get_data`) is bound to the exclusive
    /// `clipboard-backend` role AND the `clipboard.write` scope: a component
    /// with the scope but another role cannot mint clipboard transactions.
    /// Phase before scope (an unenrolled connection learns nothing).
    fn require_clipboard_backend(&self) -> Result<(), RpcErr> {
        let reg = self.state.registry.lock().expect("lock registry");
        match &reg.conns.get(&self.conn_id).expect("live connection").phase {
            Phase::Fresh => Err(RpcErr::app("NOT_ENROLLED")),
            Phase::Pending(_) => Err(RpcErr::app("PENDING_APPROVAL")),
            Phase::Active(a) if a.role == EXCLUSIVE_ROLE && a.has_scope("clipboard.write") => {
                Ok(())
            }
            Phase::Active(_) => Err(RpcErr::app("SCOPE_DENIED")),
        }
    }

    /// Removes the connection from the registry, and its pending enrollment
    /// request if any (a requester that has left has nothing left to approve).
    fn teardown(&mut self) {
        {
            let mut reg = self.state.registry.lock().expect("lock registry");
            if let Some(entry) = reg.conns.remove(&self.conn_id)
                && let Phase::Pending(request_id) = entry.phase
            {
                reg.pending.remove(&request_id);
            }
            // Reclaim any data-channel token this connection minted but that
            // never attached (an abandoned paste): otherwise the grant would
            // linger.
            reg.drop_channel_tokens_of(self.conn_id);
        }
        // A published transaction dies with its publisher, an adopted one with
        // its adopter (a grant dies with its holder - and without this, a Core
        // that never restarts would accumulate orphans no supersession sweeps).
        // Leaf lock, taken alone, after the registry's is released.
        let removed = self
            .state
            .clipboard
            .lock()
            .expect("lock clipboard")
            .revoke_owned_by(self.conn_id);
        if removed {
            self.state.clipboard_reset.notify_waiters();
        }
    }
}
