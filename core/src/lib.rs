// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! UniversalLink Core — client daemon: a local IPC server for the components,
//! server session, device identity, transfers.
//!
//! Spec: `doc/core-api.md`. The exact schemas are pinned down by the
//! integration test suite (`tests/api/`).

pub mod account_key;
mod conn;
mod connector;
mod dataplane;
mod framing;
mod http;
mod identity;
mod login;
mod rpc;
mod secrets;
mod session;
mod state;
mod transport;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use crate::connector::{Connecting, Connector, IoStream, PlainConnector, Target};
pub use crate::dataplane::{
    ALPN, Closing, FileHeader, HomeRelay, Incoming, Opening, OutgoingFile, PeerAddr, PeerTransport,
    read_offer, receive_bodies, send_transfer,
};
pub use crate::identity::load_or_generate_device_seed;
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

#[derive(Clone, Debug)]
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
    /// each failed attempt, capped. A short value only makes sense in tests.
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
}

impl Drop for CoreHandle {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.dataplane_task.abort();
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
    let state = Arc::new(AppState {
        registry: Mutex::new(Registry::new(file_token)),
        session: Mutex::new(SessionState::new(session_info.as_ref())),
        account_root: Mutex::new(account_root),
        login: Mutex::new(None),
        config_dir: config.config_dir,
        identity: device_identity,
        server_config: config.server,
        device_name: config.device_name,
        secrets: config.secret_store,
        connector: config.connector,
        transport: config.transport,
        receive_dir: config.receive_dir,
        transfers: Mutex::new(Transfers::new()),
        reconnect_base_delay: config.reconnect_base_delay,
    });

    if let Some(info) = session_info {
        start_session_task(&state, info);
    }

    let accept_state = state.clone();
    let accept_task = tokio::spawn(accept_loop(listener, accept_state));
    // The data plane listens for peers from startup — independently of the
    // server session (a peer can open a stream without us being connected to
    // the server, as long as we know its address).
    let dataplane_task = tokio::spawn(dataplane::serve(state.clone()));

    Ok(CoreHandle {
        ipc_path: config.ipc_path,
        state,
        accept_task,
        dataplane_task,
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
