// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Core's shared state: connection registry, bootstrap tokens, enrolled
//! components, pending requests, and server session state. Lock ordering:
//! `session` then `registry` (taking the registry while holding the session is
//! allowed — this is how `session.changed` goes out atomically with its
//! transition — the reverse never); `login` is never held with another;
//! `account_root`, `transfers` and `server_config` are LEAVES (never held with
//! another lock: always cloned/released beforehand). No lock across an await.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::rpc::{self, RpcErr};
use crate::session::SessionInfo;

pub type ConnId = u64;

/// An outbound message to a connection, via its bounded queue.
pub enum OutMsg {
    Frame(String),
    Close,
}

pub struct AppState {
    pub registry: Mutex<Registry>,
    pub session: Mutex<SessionState>,
    /// The account's trust root (C7): `ak_pub` to verify peers' attestations,
    /// plus our own to republish on every connection (the server, in memory,
    /// forgets it). `None` until the device has joined the account —
    /// fail-closed. Leaf lock: never held at the same time as
    /// `session`/`registry` (always cloned then released beforehand).
    pub account_root: Mutex<Option<crate::account_key::AccountRoot>>,
    /// Pending OIDC flow (login or revoke re-auth) — one at a time, the next
    /// one replaces it.
    pub login: Mutex<Option<LoginSlot>>,
    /// To remove `session.json` at logout / revocation.
    pub config_dir: PathBuf,
    /// The device's Ed25519 identity (`device.key`), cloned by the session and
    /// login tasks.
    pub identity: crate::identity::DeviceIdentity,
    /// The deployment's server + OIDC — without it, no login is possible.
    /// Behind a lock because `session.reload` swaps it live: the GUI writes a
    /// fresh `config.json`, the Core re-reads it — no process restart. LEAF
    /// lock (cloned/released before any other, never across an await).
    pub server_config: Mutex<Option<crate::ServerConfig>>,
    /// Re-reads the persisted config and returns the server it describes (or a
    /// human reason it is unusable). Injected by the daemon, which owns
    /// `config.json` parsing; `session.reload` calls it to apply what the GUI
    /// just wrote. `Ok(None)` = nothing configured.
    pub reload_server:
        std::sync::Arc<dyn Fn() -> Result<Option<crate::ServerConfig>, String> + Send + Sync>,
    /// The device's name in the directory, chosen at enrollment.
    pub device_name: String,
    /// Keyring for the durable secrets (OIDC refresh token).
    pub secrets: std::sync::Arc<dyn crate::SecretStore>,
    /// Opens the outbound streams — in the clear, or over TLS if the binary
    /// wired it in.
    pub connector: std::sync::Arc<dyn crate::Connector>,
    /// P2P data plane: iroh (daemon) or in-memory transport (tests).
    pub transport: std::sync::Arc<dyn crate::PeerTransport>,
    /// Where received files land (`files.send` from a peer). Created at the
    /// first incoming transfer. The binary points it at the user's downloads;
    /// the tests at a temporary folder.
    pub receive_dir: PathBuf,
    /// Transfers in progress (T2), cancelable in both directions. LEAF lock.
    pub transfers: Mutex<Transfers>,
    pub reconnect_base_delay: std::time::Duration,
}

/// The pending OIDC flow: its `state` (anti-CSRF — also the flow's identity)
/// and the means to stop its task when a new flow replaces it.
pub struct LoginSlot {
    pub state_param: String,
    pub abort: tokio::task::AbortHandle,
}

/// A request proxied to the server, handled by the session task.
pub struct ServerCmd {
    pub method: &'static str,
    pub params: Value,
    pub reply: tokio::sync::oneshot::Sender<Result<Value, RpcErr>>,
}

/// State of the server session, published on the IPC (`session.status`,
/// `session.changed`, `devices.*`).
pub struct SessionState {
    /// `session.json` present and readable at startup (or not yet abandoned).
    /// Says nothing about the connection.
    pub logged_in: bool,
    /// The `account` field of session.json, replayed as-is.
    pub account: Option<Value>,
    /// Authenticated AND directory snapshotted: connected ⇒ cache primed.
    pub server_connected: bool,
    /// The Core's device_id in the directory.
    pub own_device_id: Option<String>,
    /// The account's directory: the last known snapshot, maintained by the
    /// `device.*` events. `None` until a snapshot has succeeded since startup —
    /// there is then nothing honest to serve.
    pub devices: Option<BTreeMap<String, Value>>,
    /// Sender toward the session task — present when the connection is
    /// established (the `devices.rename`… proxies go through it).
    pub server_tx: Option<mpsc::Sender<ServerCmd>>,
    /// To stop the session task at logout.
    pub session_abort: Option<tokio::task::AbortHandle>,
}

impl SessionState {
    pub fn new(info: Option<&SessionInfo>) -> SessionState {
        SessionState {
            logged_in: info.is_some(),
            account: info.and_then(|i| i.account.clone()),
            server_connected: false,
            own_device_id: info.map(|i| i.device_id.clone()),
            devices: None,
            server_tx: None,
            session_abort: None,
        }
    }

    /// The result of `session.status` — also the payload of `session.changed`.
    pub fn status_record(&self) -> Value {
        let mut v = json!({
            "logged_in": self.logged_in,
            "server_connected": self.server_connected,
        });
        if let Some(account) = &self.account {
            v["account"] = account.clone();
        }
        v
    }

    /// Forgets the session (logout, revocation); returns the notification
    /// payload and the task's stop handle, if there is one.
    pub fn forget(&mut self) -> (Value, Option<tokio::task::AbortHandle>) {
        self.logged_in = false;
        self.account = None;
        self.server_connected = false;
        self.own_device_id = None;
        self.devices = None;
        self.server_tx = None;
        let abort = self.session_abort.take();
        (self.status_record(), abort)
    }
}

/// A transfer in progress, cancelable. No `AbortHandle`: we signal the task
/// rather than kill it, so it cleans up properly (stream reset, `.part`
/// temporaries erased, terminal notification emitted ONCE — by the task itself,
/// never by `files.cancel`).
pub struct TransferEntry {
    /// Signaled by `files.cancel`. `notify_one`: a signal posted before the
    /// task waits is not lost (a memorized permit).
    pub cancel: std::sync::Arc<tokio::sync::Notify>,
}

/// Registry of transfers (T2), outgoing and incoming. LEAF lock (see the module
/// header): never held with `session`/`registry`/`account_root`.
pub struct Transfers {
    pub entries: HashMap<String, TransferEntry>,
}

impl Transfers {
    pub fn new() -> Transfers {
        Transfers {
            entries: HashMap::new(),
        }
    }

    /// Mints a `transfer_id` and registers the transfer; returns its
    /// cancellation token (which the task `select!`s on, and `files.cancel`
    /// triggers). The id is RANDOM, not sequential: a component cannot guess and
    /// cancel another's transfer by enumerating `t_1`, `t_2`…
    pub fn register(&mut self) -> (String, std::sync::Arc<tokio::sync::Notify>) {
        let id = format!("t_{}", random_hex(8));
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        self.entries.insert(
            id.clone(),
            TransferEntry {
                cancel: cancel.clone(),
            },
        );
        (id, cancel)
    }
}

/// A device record as served on the IPC: the server's, enriched with `is_self`
/// (doc/core-api.md, "devices.*").
pub fn enrich_device(record: &Value, own_device_id: Option<&str>) -> Value {
    let mut v = record.clone();
    let is_self =
        own_device_id.is_some() && record.get("device_id").and_then(Value::as_str) == own_device_id;
    v["is_self"] = json!(is_self);
    v
}

/// Rights carried by a spawn token (bootstrap path B), single-use.
pub struct SpawnGrant {
    pub role: String,
    pub scopes: Vec<String>,
}

/// An enrolled third-party component: the token persists beyond the connection.
/// (v1: in memory only — a Core restart forgets the enrollment.)
pub struct Enrolled {
    pub component_id: String,
    pub token: String,
    pub name: String,
    pub role: String,
    pub scopes: Vec<String>,
}

/// An enrollment request awaiting the user's decision.
pub struct PendingRequest {
    pub request_id: String,
    pub conn_id: ConnId,
    pub name: String,
    pub role: String,
    pub scopes: Vec<String>,
    /// `peer_info` as broadcast (pid… drawn from the peer credentials).
    pub peer_info: Value,
}

impl PendingRequest {
    pub fn record(&self) -> Value {
        json!({
            "request_id": self.request_id,
            "name": self.name,
            "role": self.role,
            "scopes": self.scopes,
            "peer_info": self.peer_info,
        })
    }
}

/// A connection whose `hello` has been accepted.
pub struct Active {
    pub component_id: String,
    pub name: String,
    pub role: String,
    /// Granted scopes, in the order of the request.
    pub scopes: Vec<String>,
    /// Current subscription (`events.subscribe` replaces it). Read by the topic
    /// broadcast, wired with the server session (next building block).
    pub topics: Vec<String>,
}

impl Active {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

pub enum Phase {
    /// Connected, no `hello` accepted.
    Fresh,
    /// `hello` without a token: request queued, referenced by its request_id.
    Pending(String),
    Active(Active),
}

pub struct ConnEntry {
    pub tx: mpsc::Sender<OutMsg>,
    pub phase: Phase,
}

pub struct Registry {
    next_conn_id: ConnId,
    /// Trust root A: the contents of `ipc-token`, reusable.
    pub file_token: String,
    /// Trust root B: tokens passed at spawn, consumed at the first accepted
    /// `hello`.
    pub spawn_tokens: HashMap<String, SpawnGrant>,
    /// persistent token → component_id.
    pub enrolled_tokens: HashMap<String, String>,
    /// component_id → enrolled third-party component. BTreeMap: stable
    /// inventory.
    pub enrolled: BTreeMap<String, Enrolled>,
    /// request_id → request. BTreeMap: stable `components.pending`.
    pub pending: BTreeMap<String, PendingRequest>,
    pub conns: HashMap<ConnId, ConnEntry>,
    /// The Core is shutting down (drop of the `CoreHandle`). Set and read under
    /// this lock: a connection that registers sees either `false` (it will
    /// receive the Close from the sweep) or `true` (it gives up on its own) —
    /// no window where it would survive the shutdown.
    pub shutdown: bool,
}

impl Registry {
    pub fn new(file_token: String) -> Registry {
        Registry {
            next_conn_id: 0,
            file_token,
            spawn_tokens: HashMap::new(),
            enrolled_tokens: HashMap::new(),
            enrolled: BTreeMap::new(),
            pending: BTreeMap::new(),
            conns: HashMap::new(),
            shutdown: false,
        }
    }

    pub fn new_conn_id(&mut self) -> ConnId {
        self.next_conn_id += 1;
        self.next_conn_id
    }

    /// Is the exclusive role already held by an active connection?
    pub fn role_taken(&self, role: &str) -> bool {
        self.conns
            .values()
            .any(|c| matches!(&c.phase, Phase::Active(a) if a.role == role))
    }

    /// Pushes a notification to all active connections carrying `scope` — with
    /// no subscription: it is the duty attached to the scope
    /// (`component.pending` → `components.approve`).
    pub fn notify_scope(&self, scope: &str, method: &str, params: &Value) {
        let frame = rpc::notification(method, params);
        for entry in self.conns.values() {
            if let Phase::Active(a) = &entry.phase
                && a.has_scope(scope)
            {
                // Queue full or dying connection: too bad for it, the snapshot
                // (`components.pending`) catches up.
                let _ = entry.tx.try_send(OutMsg::Frame(frame.clone()));
            }
        }
    }

    /// Pushes a notification to the active connections subscribed to `topic`
    /// (the scope was verified at subscription; a connection's scopes do not
    /// change).
    pub fn notify_topic(&self, topic: &str, method: &str, params: &Value) {
        let frame = rpc::notification(method, params);
        for entry in self.conns.values() {
            if let Phase::Active(a) = &entry.phase
                && a.topics.iter().any(|t| t == topic)
            {
                // Queue full or dying connection: too bad for it, the snapshots
                // (`devices.list`, `session.status`) catch up.
                let _ = entry.tx.try_send(OutMsg::Frame(frame.clone()));
            }
        }
    }

    /// Sends a notification to a specific connection.
    pub fn notify_conn(&self, conn_id: ConnId, method: &str, params: &Value) {
        if let Some(entry) = self.conns.get(&conn_id) {
            let _ = entry
                .tx
                .try_send(OutMsg::Frame(rpc::notification(method, params)));
        }
    }

    /// Requests the closure of a connection (curt: EOF on the peer side).
    pub fn close_conn(&self, conn_id: ConnId) {
        if let Some(entry) = self.conns.get(&conn_id) {
            let _ = entry.tx.try_send(OutMsg::Close);
        }
    }
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut buf);
    hex::encode(buf)
}
