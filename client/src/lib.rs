// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! IPC client for the Core, used by official components written in Rust (GUI,
//! tray, clipboard manager): managed connection, multiplexed requests,
//! relayed notifications.
//!
//! Protocol spec: `doc/core-api.md`. The crate's exact contract is frozen
//! by the integration suite (`tests/api/`), which consumes it against the
//! real `universallink-core` lib.
//!
//! Model: [`spawn`] starts a task that maintains the connection to the Core
//! (hello, subscriptions, reconnection with backoff) and publishes [`Event`]s;
//! the [`Client`] (clonable) carries the requests. Fail-closed everywhere:
//! while offline, requests fail immediately, and the state shown by the
//! consumer must follow the connection events.

mod channel;
mod conn;
mod framing;
mod transport;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

pub use channel::{ChannelError, ConsumerChannel, ErrorCode, ProviderChannel};
pub use conn::Client;

/// Major version of the IPC API this crate implements. A Core announcing
/// anything else is incompatible: the client stops permanently.
pub const API_VERSION: u64 = 1;

/// Where the hello's token comes from.
#[derive(Clone, Debug)]
pub enum TokenSource {
    /// Root A: file token (`ipc-token`), re-read from disk on EVERY
    /// connection attempt — the Core regenerates it on each startup.
    File(PathBuf),
    /// Root B: ephemeral token passed at spawn time by the supervisor.
    /// Single-use on the Core side: once consumed, a reconnection with the
    /// same token will fail (the supervisor restarts the component).
    Spawn(String),
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// The Core's listening endpoint — unix: UDS socket path; windows: full
    /// named pipe name.
    pub ipc_path: PathBuf,
    pub token: TokenSource,
    /// Identity declared at hello time.
    pub name: String,
    pub version: String,
    pub role: String,
    pub scopes: Vec<String>,
    /// `events.subscribe` topics, subscribed on every (re)connection before
    /// `Event::Connected`. Empty: no subscription.
    pub topics: Vec<String>,
    /// Core→component request methods this component serves (e.g.
    /// `clipboard.get_data` for the clipboard backend). A served method is
    /// surfaced as [`Event::Request`], to be answered with [`Client::respond`]
    /// / [`Client::respond_error`]. Any method NOT in this list is refused with
    /// `-32601` automatically — so an empty list (the default) is the pure
    /// client behavior the tray and GUI rely on.
    pub served_methods: Vec<String>,
    /// Base of the exponential reconnection backoff — doubled on each failure,
    /// capped at 60 s, reset to the base after a successful establishment.
    pub reconnect_base_delay: Duration,
    /// Request timeout ([`RequestError::Timeout`] beyond it).
    pub request_timeout: Duration,
}

/// What the connection task publishes to the consumer.
#[derive(Clone, Debug)]
pub enum Event {
    /// Connection established: hello accepted (and topics subscribed). The
    /// Core's state must be resynchronized via the snapshot methods.
    Connected {
        granted_scopes: Vec<String>,
        api_version: u64,
    },
    /// Connection lost: the reconnection cycle resumes.
    Disconnected,
    /// Core notification, relayed as-is.
    Notification { method: String, params: Value },
    /// A Core→component request whose method is in `served_methods`. Answer it
    /// with [`Client::respond`] / [`Client::respond_error`], passing `id` back
    /// verbatim. Responding is free-standing (it need not be immediate): the
    /// clipboard backend replies to `clipboard.get_data` only after streaming
    /// the blob over a provider channel.
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    /// The Core speaks a different major version: permanent client shutdown.
    Incompatible { api_version: u64 },
}

/// Opaque handle to an incoming request, carried from [`Event::Request`] to
/// [`Client::respond`]. It is bound to the connection that delivered the
/// request: responding after that connection dropped (and reconnected) fails
/// with [`RequestError::Disconnected`] rather than sending a stale id onto a
/// fresh connection.
#[derive(Clone, Debug)]
pub struct RequestId {
    pub(crate) generation: u64,
    pub(crate) id: Value,
}

/// JSON-RPC error relayed as-is.
#[derive(Clone, Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    /// Application code (`error.data.code`) — `SERVER_UNREACHABLE`, etc.
    pub data_code: Option<String>,
}

#[derive(Debug)]
pub enum RequestError {
    /// No connection to the Core (fail-closed: nothing is queued).
    NotConnected,
    /// No response within `request_timeout` (the connection survives).
    Timeout,
    /// Connection lost during the request — its fate is unknown.
    Disconnected,
    Rpc(RpcError),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::NotConnected => write!(f, "no connection to the Core"),
            RequestError::Timeout => write!(f, "no response from the Core in time"),
            RequestError::Disconnected => {
                write!(f, "connection to the Core lost during the request")
            }
            RequestError::Rpc(e) => match &e.data_code {
                Some(code) => write!(f, "{code}: {}", e.message),
                None => write!(f, "error {}: {}", e.code, e.message),
            },
        }
    }
}

impl std::error::Error for RequestError {}

/// Starts the client: the connection task lives as long as a [`Client`] exists
/// (or until a version incompatibility). The event channel is bounded: a
/// consumer that stops reading eventually suspends reading from the socket —
/// the Core will close (fail-closed), never unbounded memory.
pub fn spawn(config: ClientConfig) -> (Client, mpsc::Receiver<Event>) {
    conn::spawn(config)
}
