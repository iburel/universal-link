// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! 1Device Core — client daemon: a local IPC server for the components,
//! server session, device identity, transfers.
//!
//! Spec: `doc/core-api.md`. The exact schemas are pinned down by the
//! integration test suite (`tests/api/`).

pub mod account_key;
mod clipboard;
mod clipnet;
mod conn;
mod connector;
mod datachannel;
mod dataplane;
pub mod directory;
mod dirsync;
mod discover;
mod framing;
mod http;
mod identity;
mod login;
mod pairing;
mod relays;
mod rpc;
mod secrets;
mod session;
mod state;
mod transport;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use crate::connector::{Connecting, Connector, IoStream, PlainConnector, Target};
pub use crate::dataplane::{
    ALPN, Closing, FileHeader, HomeRelay, Incoming, Listening, NO_DIRECT_PATH, Opening,
    OutgoingFile, PeerAddr, PeerTransport, read_offer, receive_bodies, send_transfer,
};
pub use crate::directory::Reach;
pub use crate::identity::load_or_generate_device_seed;
pub use crate::pairing::{
    LAN_PAYLOAD_TAG as PAIRING_LAN_CODE_TAG, PAYLOAD_TAG as PAIRING_CODE_TAG,
};
pub use crate::secrets::{FileSecretStore, SecretStore};
use crate::state::{AppState, Registry, SessionState, SpawnGrant, Transfers, random_hex};

/// Major version of the IPC API, returned by `hello`.
pub const API_VERSION: u64 = 1;

/// The deployment's server and its IdP: what is needed for a login
/// (`session.login`). The Core is the public OIDC client (PKCE, no secret); the
/// issuer and the client_id are the ones configured on the server side.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// The server's WebSocket URL (`wss://…/ws` — `ws://` in tests).
    pub url: String,
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    /// The OIDC client secret, sent at the code exchange and at refresh WHEN it
    /// is present. Normally unnecessary for a public client (PKCE) — but Google
    /// REQUIRES it even under PKCE, including for a "Desktop app" client (its
    /// secret is then not confidential, it is distributed with the app). `None`
    /// for IdPs that conform to RFC 7636.
    pub oidc_client_secret: Option<String>,
}

// No `Debug`: `reload_server` is a closure (not `Debug`), and nothing prints a
// `Config` — it is an input, built once and handed to `spawn`.
#[derive(Clone)]
pub struct Config {
    /// IPC listening point — unix: the UDS socket path; windows: the full name
    /// of the named pipe (`\\.\pipe\…`).
    pub ipc_path: PathBuf,
    /// The Core's config folder; the file token (`ipc-token`, 0600) is
    /// rewritten there at every startup.
    pub config_dir: PathBuf,
    /// Server + OIDC — `None`: Core never configured, login fails with
    /// `SERVER_UNREACHABLE` (an existing session, by contrast, carries its own
    /// URL).
    pub server: Option<ServerConfig>,
    /// Re-reads the persisted config on demand, for `session.reload`: `Ok(None)`
    /// = nothing configured, `Err` = a human reason it is unusable. The daemon
    /// supplies it (it owns `config.json` parsing); tests pass a trivial one.
    pub reload_server: Arc<dyn Fn() -> Result<Option<ServerConfig>, String> + Send + Sync>,
    /// The human reason the persisted config is faulty at startup, when it is:
    /// the same sentence `reload_server` would return as `Err`. Carried into
    /// `session.status` (`problem`), because the interface is the only place
    /// the user will ever read it; the startup log alone reaches nobody. It
    /// does NOT make the Core unconfigured: a faulty setting is simply not
    /// applied and `server` keeps whatever the parse could still honor.
    pub config_problem: Option<String>,
    /// The device's name in the directory, chosen at enrollment (the binary
    /// will pass the hostname).
    pub device_name: String,
    /// Keyring for the durable secrets — `FileSecretStore` as a fallback, the
    /// binary will wire in the OS keyring.
    pub secret_store: Arc<dyn SecretStore>,
    /// Opens the outbound streams (server WS, IdP HTTP). `PlainConnector` only
    /// speaks in the clear; the binary wires in the TLS connector, because no
    /// TLS stack cross-compiles from this crate (see `connector`).
    pub connector: Arc<dyn Connector>,
    /// P2P data plane (iroh). The binary wires in the iroh impl (compiled
    /// natively), the tests an in-memory transport — same reason as the
    /// connector (see `dataplane`).
    pub transport: Arc<dyn PeerTransport>,
    /// Where received files land (`files.send` from a peer) — the binary points
    /// it at the user's downloads (overridable), the tests at a temporary
    /// folder. Created at the first incoming transfer.
    pub receive_dir: PathBuf,
    /// Base of the exponential reconnection backoff to the server — doubled at
    /// each failed attempt, and capped at 64 times itself
    /// (`session::RECONNECT_MAX_MULTIPLE`). So this single value sets both how
    /// fast the Core comes back from a blip and how long it may sleep between
    /// attempts when the network is gone: a daemon on a wired network asks for
    /// seconds, a phone (whose network goes away whenever the app does) for
    /// fractions of one, the tests for milliseconds.
    pub reconnect_base_delay: std::time::Duration,
}

/// Why the Core did not start. `AlreadyRunning` is not a failure: a Core is
/// already listening for this user. The library reports it without concluding
/// — it is the binary that decides to exit (in-process, an `exit()` here would
/// kill the test suite).
#[derive(Debug)]
pub enum SpawnError {
    AlreadyRunning,
    Failed(anyhow::Error),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::AlreadyRunning => {
                write!(f, "a Core is already running for this user")
            }
            SpawnError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SpawnError {}

impl From<std::io::Error> for SpawnError {
    fn from(e: std::io::Error) -> SpawnError {
        SpawnError::Failed(e.into())
    }
}

impl From<anyhow::Error> for SpawnError {
    fn from(e: anyhow::Error) -> SpawnError {
        SpawnError::Failed(e)
    }
}

pub struct CoreHandle {
    ipc_path: PathBuf,
    state: Arc<AppState>,
    accept_task: tokio::task::JoinHandle<()>,
    /// The data plane accept loop (iroh) — alive as long as the Core runs,
    /// `abort()`ed at drop (it holds an `Arc<AppState>`).
    dataplane_task: tokio::task::JoinHandle<()>,
    /// LAN presence → `device.updated` broadcasts. Same lifecycle as the
    /// accept loop; already finished on a transport without LAN discovery.
    lan_presence_task: tokio::task::JoinHandle<()>,
    /// Directory exchanges with the account's other devices (`dirsync`). Same
    /// lifecycle: it holds an `Arc<AppState>` and is `abort()`ed at drop.
    dirsync_task: tokio::task::JoinHandle<()>,
    /// Keeps our own record's signed reach hints in step with the transport.
    /// Same lifecycle; already finished on a transport with nothing to watch.
    reach_task: tokio::task::JoinHandle<()>,
    /// Dropped at `drop` — hence before a restart reclaims the socket.
    _instance: transport::InstanceGuard,
}

impl CoreHandle {
    pub fn ipc_path(&self) -> &Path {
        &self.ipc_path
    }

    /// Path B of the trust bootstrap: an ephemeral (single-use) token that the
    /// supervisor passes to the components it spawns. The hello will have to
    /// present this role, and scopes included among these.
    pub fn mint_spawn_token(&self, role: &str, scopes: &[&str]) -> String {
        let token = random_hex(32);
        let mut reg = self.state.registry.lock().expect("lock registry");
        reg.spawn_tokens.insert(
            token.clone(),
            SpawnGrant {
                role: role.to_string(),
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
            },
        );
        token
    }

    /// Removes a still-unused spawn grant. The supervisor calls it when the
    /// child dies without having presented itself: without this, an activation
    /// token would outlive its recipient until the Core shuts down, and each
    /// restart would leave one more behind it.
    pub fn revoke_spawn_token(&self, token: &str) {
        self.state
            .registry
            .lock()
            .expect("lock registry")
            .spawn_tokens
            .remove(token);
    }

    /// Resolves when a component asked the Core to stop (`system.shutdown` — the
    /// tray's Quit). The binary awaits this alongside the OS signals, then runs
    /// its usual teardown (components, then IPC, then the data plane). The Core
    /// is relaunched by opening the GUI, which respawns it.
    pub async fn shutdown_requested(&self) {
        self.state.shutdown_request.notified().await;
    }
}

impl Drop for CoreHandle {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.dataplane_task.abort();
        self.lan_presence_task.abort();
        self.dirsync_task.abort();
        self.reach_task.abort();
        // Closes the established IPC connections: a cleanly stopped Core does
        // not leave its components on a mute socket (in a separate process the
        // problem does not exist, in an in-process lib the tasks would leak).
        // `shutdown` is set under the same lock as the sweep: a connection
        // accepted but not yet registered will give up on its own by reading it
        // at registration.
        let mut reg = self.state.registry.lock().expect("lock registry");
        reg.shutdown = true;
        for entry in reg.conns.values() {
            if entry.tx.try_send(crate::state::OutMsg::Close).is_err() {
                // Queue momentarily full with a peer that is reading: the Close
                // would be lost (WRITE_TIMEOUT only covers the peer that no
                // longer reads). We replay it asynchronously if a runtime still
                // exists — otherwise the process is shutting down, and the OS
                // will close.
                let tx = entry.tx.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let _ = tx.send(crate::state::OutMsg::Close).await;
                    });
                }
            }
        }
        drop(reg);
        // Drop every clipboard transaction and cut the open consumer channels
        // (data-channel connections are not in `conns`, so the sweep above does
        // not reach them): the right to read ends when the Core stops.
        self.state
            .clipboard
            .lock()
            .expect("lock clipboard")
            .clear_all();
        self.state.clipboard_reset.notify_waiters();
        // The session and login tasks are held by the state (logout and flow
        // replacement go through it): stopped via their handles.
        if let Some(abort) = self
            .state
            .session
            .lock()
            .expect("lock session")
            .session_abort
            .take()
        {
            abort.abort();
        }
        if let Some(slot) = self.state.login.lock().expect("lock login").take() {
            slot.abort.abort();
        }
    }
}

/// Starts the Core; returns once the IPC socket is listening.
pub async fn spawn(config: Config) -> Result<CoreHandle, SpawnError> {
    std::fs::create_dir_all(&config.config_dir)?;
    // Listening FIRST: it carries the mutual exclusion. Writing `ipc-token`
    // beforehand would amount, for a second Core, to revoking the first's token
    // out from under it — its components would reconnect with a secret no one
    // recognizes anymore — only to then give up on starting.
    let (listener, instance) = transport::bind(&config.ipc_path).map_err(|e| match e {
        transport::BindError::AlreadyRunning => SpawnError::AlreadyRunning,
        transport::BindError::Io(e) => SpawnError::Failed(e.into()),
    })?;
    // Trust root A: regenerated at every startup — a leaked secret does not
    // survive the next launch.
    let file_token = random_hex(32);
    write_private_file(&config.config_dir.join("ipc-token"), &file_token)?;
    // The device identity precedes the session: it is born at first startup,
    // login merely enrolls it.
    let device_identity = identity::DeviceIdentity::load_or_generate(&config.config_dir)?;
    // The account's trust root (C7): present if this device has already joined
    // the account (`account.setup`/`account.join`). Absent → fail-closed: the
    // data plane authorizes and opens no stream (see `dataplane`).
    //
    // The persisted root attests ONE specific node_id. If `device.key` changed
    // under our feet (regenerated after a deletion), the attestation is worth
    // nothing anymore: we IGNORE the root (peers would reject it anyway) rather
    // than believe ourselves part of the account and republish a stale
    // attestation — a silent state with no way out via the API. Ignored,
    // `account.join` can re-attest the new node_id.
    let account_root = account_key::load(&config.config_dir).filter(|root| {
        let ok = account_key::verify(&root.ak_pub, &device_identity.node_id(), &root.attestation);
        if !ok {
            tracing::warn!(
                "account-key.json does not attest the local node_id (device.key changed?): root ignored, join the account again"
            );
        }
        ok
    });

    let session_info = session::read_session_file(&config.config_dir);
    // The stored directory vouches for a Core that belongs somewhere: a session,
    // or the account's trust root — a device that joined the account without
    // ever logging in has no server to snapshot from, so that file is not a
    // cache of a directory, it IS the directory. Neither: nothing is served, no
    // matter what the disk says. Stale or corrupt loads as nothing — fail-closed,
    // exactly as before the file existed.
    let stored_devices = match (&session_info, &account_root) {
        (None, None) => None,
        // Only a configured server could ever refresh it, and that is what
        // makes expiry meaningful (see `directory`).
        _ => directory::load(&config.config_dir, config.server.is_some()),
    };
    // Whom the account has struck off (signed tombstones). Read unconditionally
    // and never expired: a revocation is permanent, and it is not the session's
    // to lose — the struck-off device keeps a valid attestation for good.
    let revoked = directory::load_revoked(&config.config_dir);
    let state = Arc::new(AppState {
        registry: Mutex::new(Registry::new(file_token)),
        session: Mutex::new(SessionState::new(session_info.as_ref())),
        account_root: Mutex::new(account_root),
        login: Mutex::new(None),
        pairing: Mutex::new(None),
        config_dir: config.config_dir,
        identity: device_identity,
        server_config: Mutex::new(config.server),
        config_problem: Mutex::new(config.config_problem),
        reload_server: config.reload_server,
        device_name: config.device_name,
        secrets: config.secret_store,
        connector: config.connector,
        transport: config.transport,
        receive_dir: config.receive_dir,
        transfers: Mutex::new(Transfers::new()),
        clipboard: Mutex::new(crate::clipboard::ClipboardState::new()),
        clipboard_reset: tokio::sync::Notify::new(),
        dirsync_wake: tokio::sync::Notify::new(),
        reach_wake: tokio::sync::Notify::new(),
        reconnect_base_delay: config.reconnect_base_delay,
        shutdown_request: tokio::sync::Notify::new(),
    });
    // Cloned out of the leaf lock before taking `session` (lock ordering).
    let root = state
        .account_root
        .lock()
        .expect("lock account_root")
        .clone();
    {
        // Seeded before any task exists — no reader can race it. The first
        // successful session setup replaces the records with the live snapshot;
        // the tombstones, nothing replaces.
        let mut s = state.session.lock().expect("lock session");
        s.revoked = revoked;
        if let Some(mut devices) = stored_devices {
            // A struck-off device does not come back from the disk. This is what
            // the store carries INSTEAD of a freshness bound where no server
            // could ever refresh it (see `directory`).
            if let Some(root) = &root {
                devices.retain(|_, record| {
                    record
                        .get("node_id")
                        .and_then(serde_json::Value::as_str)
                        .is_none_or(|node_id| !s.barred(&root.ak_pub, node_id))
                });
            }
            s.devices = Some(devices);
        }
        // A device in the account knows at least ITSELF — session or not, server
        // or not. Without this a Core that joined the account and never logged in
        // would answer `SERVER_UNREACHABLE` to a component asking who is around,
        // while holding everything needed to say "me".
        //
        // Deliberately NOT persisted: writing the store here would refresh its
        // `saved_at` at every startup and silently extend the staleness bound of
        // records the server may have revoked meanwhile.
        if let Some(root) = &root {
            s.adopt_own(crate::state::OwnDevice {
                identity: &state.identity,
                name: &state.device_name,
                attestation: &root.attestation,
            });
        }
    }

    if let Some(info) = session_info {
        start_session_task(&state, info);
    }

    // The deployment's cached relay announcement (#105), handed to the
    // transport BEFORE anything can bind it: a Core that boots with the
    // server down still binds with the operator's relays - and keeps
    // honoring their announced role (#88). An empty cache announces nothing,
    // which is exactly what an off default should hear.
    let cached = relays::load(&state.config_dir);
    if !cached.relays.is_empty() {
        state
            .transport
            .announce_relays(&cached.relays, cached.relay_max_payload);
    }

    let accept_state = state.clone();
    let accept_task = tokio::spawn(accept_loop(listener, accept_state));
    // The data plane listens for peers from startup — independently of the
    // server session (a peer can open a stream without us being connected to
    // the server, as long as we know its address).
    let dataplane_task = tokio::spawn(dataplane::serve(state.clone()));
    // LAN presence: relays mDNS visibility changes onto the `devices` topic.
    let lan_presence_task = tokio::spawn(dataplane::watch_lan_presence(state.clone()));
    // Directory sync: tells the account's other devices whom we know, and takes
    // in whom they know. Independent of the server session, for the same reason
    // the accept loop is.
    let dirsync_task = tokio::spawn(dirsync::run(state.clone()));
    // Own reach: re-signs our record when the transport's addresses (or its
    // chosen relay) move, so the hints the account holds about us are
    // never staler than one watcher wakeup.
    let reach_task = tokio::spawn(dataplane::watch_own_reach(state.clone()));

    Ok(CoreHandle {
        ipc_path: config.ipc_path,
        state,
        accept_task,
        dataplane_task,
        lan_presence_task,
        dirsync_task,
        reach_task,
        _instance: instance,
    })
}

/// Starts the session task for `info` and retains its stop handle (for the
/// logout). Called at startup (session.json present) and at the completion of a
/// login.
pub(crate) fn start_session_task(state: &Arc<AppState>, info: session::SessionInfo) {
    let task = tokio::spawn(session::run(state.clone(), info));
    state.session.lock().expect("lock session").session_abort = Some(task.abort_handle());
}

async fn accept_loop(mut listener: transport::Listener, state: Arc<AppState>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::spawn(conn::run(state.clone(), stream, peer));
            }
            Err(e) => {
                // Accept error (descriptors exhausted…): we do not die, we
                // pause before retrying — the IPC must survive.
                tracing::warn!(error = %e, "IPC accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Writes a secret file (`ipc-token`, `device.key`) as 0600: readable by the
/// Core's trust perimeter (the user), and no one else.
pub(crate) fn write_private_file(path: &Path, content: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    use std::io::Write;
    let mut file = options.open(path)?;
    // `mode` only applies at creation: tighten a pre-existing file too.
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(content.as_bytes())?;
    Ok(())
}
