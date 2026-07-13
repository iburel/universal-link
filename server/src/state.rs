// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Shared state: device directory, active connections, broadcast.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use crate::Config;
use crate::oidc::OidcValidator;
use crate::store::{DirectoryStore, DurableDevice, DurableState};

/// Message pushed to a connection's write task.
pub enum OutMsg {
    /// JSON-RPC text frame (response or notification).
    Notify(String),
    /// Heartbeat ping.
    Ping,
    /// Close the connection with this reason in the close frame.
    Close(&'static str),
}

pub type ConnId = u64;

pub struct AppState {
    pub config: Config,
    pub oidc: OidcValidator,
    pub registry: Mutex<Registry>,
    /// Durable state persistence: memory store in ephemeral mode (`spawn`),
    /// disk store in deployment (see `server-daemon`).
    store: Arc<dyn DirectoryStore>,
}

impl AppState {
    pub fn new(config: Config, store: Arc<dyn DirectoryStore>, initial: DurableState) -> AppState {
        AppState {
            oidc: OidcValidator::new(&config.oidc),
            registry: Mutex::new(Registry::from_durable(initial)),
            config,
            store,
        }
    }

    /// Persists the current durable state. Best-effort: a failure is logged but
    /// does not interrupt the session (the in-memory state stays correct; only
    /// survival across a restart is at stake). The save is done UNDER the
    /// directory lock — writes are thereby serialized, and the snapshot written
    /// always reflects the most recent state (the dataset is small).
    pub fn persist(&self) {
        let reg = self.registry.lock().expect("lock registry");
        if let Err(e) = self.store.save(&reg.durable_snapshot()) {
            tracing::error!(error = format!("{e:#}"), "directory persistence failed");
        }
    }
}

#[derive(Default)]
pub struct Registry {
    /// Directory, indexed by device_id.
    pub devices: HashMap<String, DeviceEntry>,
    /// Ids struck off the directory: a re-authentication must answer
    /// DEVICE_REVOKED, not DEVICE_UNKNOWN.
    pub revoked: HashSet<String>,
    next_conn_id: ConnId,
}

pub struct DeviceEntry {
    /// Owner account (OIDC `sub` — the issuer is fixed by the config).
    pub account: String,
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub node_id: String,
    pub relay_url: Option<String>,
    pub status: Option<String>,
    pub last_seen: Option<String>,
    /// Account attestation (C7): signature by the account key binding this
    /// `node_id` to the account, published by the device (`presence.update`).
    /// Opaque to the server — carried and rebroadcast, never interpreted; it's
    /// the peer that verifies it. Survives going offline (bound to `node_id`,
    /// stable), unlike `relay_url` (specific to the connection).
    pub attestation: Option<String>,
    /// Current connection: one device = at most one connection.
    pub conn: Option<(ConnId, Sender<OutMsg>)>,
}

impl DeviceEntry {
    /// The public device record (doc/server-api.md).
    pub fn record(&self) -> Value {
        json!({
            "device_id": self.device_id,
            "name": self.name,
            "platform": self.platform,
            "node_id": self.node_id,
            "relay_url": self.relay_url,
            "attestation": self.attestation,
            "online": self.conn.is_some(),
            "status": self.status,
            "last_seen": self.last_seen,
        })
    }
}

impl Registry {
    /// Rebuilds the directory from durable state at startup. Everything
    /// ephemeral (connection, relay_url, presence, last_seen) starts from
    /// scratch: it republishes itself when devices reconnect.
    fn from_durable(state: DurableState) -> Registry {
        let devices = state
            .devices
            .into_iter()
            .map(|d| {
                (
                    d.device_id.clone(),
                    DeviceEntry {
                        account: d.account,
                        device_id: d.device_id,
                        name: d.name,
                        platform: d.platform,
                        node_id: d.node_id,
                        attestation: d.attestation,
                        relay_url: None,
                        status: None,
                        last_seen: None,
                        conn: None,
                    },
                )
            })
            .collect();
        Registry {
            devices,
            revoked: state.revoked.into_iter().collect(),
            next_conn_id: 0,
        }
    }

    /// The durable subset of the directory, to be persisted.
    fn durable_snapshot(&self) -> DurableState {
        DurableState {
            devices: self
                .devices
                .values()
                .map(|d| DurableDevice {
                    account: d.account.clone(),
                    device_id: d.device_id.clone(),
                    name: d.name.clone(),
                    platform: d.platform.clone(),
                    node_id: d.node_id.clone(),
                    attestation: d.attestation.clone(),
                })
                .collect(),
            revoked: self.revoked.iter().cloned().collect(),
        }
    }

    pub fn new_conn_id(&mut self) -> ConnId {
        self.next_conn_id += 1;
        self.next_conn_id
    }

    pub fn account_devices<'a>(
        &'a self,
        account: &'a str,
    ) -> impl Iterator<Item = &'a DeviceEntry> {
        self.devices.values().filter(move |d| d.account == account)
    }

    /// Broadcasts a notification to the account's authenticated connections,
    /// except `except` — the connection that caused the change has the response.
    ///
    /// The send never blocks: a full queue signals a consumer that is too slow,
    /// which its own loop will disconnect. Losing a notification for it is
    /// inconsequential — it resyncs on reconnect.
    pub fn broadcast(&self, account: &str, except: ConnId, method: &str, params: Value) {
        let frame = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
        for device in self.account_devices(account) {
            if let Some((conn_id, tx)) = &device.conn
                && *conn_id != except
            {
                let _ = tx.try_send(OutMsg::Notify(frame.clone()));
            }
        }
    }
}

/// Current timestamp in RFC 3339 UTC (second precision).
pub fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string()
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut buf);
    hex::encode(buf)
}
