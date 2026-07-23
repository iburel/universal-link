// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! UniversalLink server — control plane: authentication (OIDC + device keys),
//! device directory, presence, iroh dial info.
//!
//! Spec: `doc/server-api.md`. The exact schemas are frozen by the integration
//! test suite (`tests/api.rs`).

mod conn;
mod oidc;
mod rpc;
mod state;
mod store;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;

use crate::state::AppState;
pub use crate::store::{DirectoryStore, DurableDevice, DurableState, MemoryStore};

/// Major version of the public API, returned by `auth.enroll` and
/// `auth.authenticate`.
pub const API_VERSION: u64 = 1;

#[derive(Clone, Debug)]
pub struct Config {
    /// Listen address (WebSocket on `/ws`, health on `GET /health`).
    /// TLS is terminated upstream (reverse proxy) — the server listens in the clear.
    pub bind_addr: SocketAddr,
    pub oidc: OidcConfig,
    /// WebSocket ping interval.
    pub heartbeat_interval: Duration,
    /// Number of missed pongs before closing the connection (→ offline).
    pub heartbeat_max_missed: u32,
    /// Lifetime of a nonce issued by `auth.challenge`.
    pub nonce_ttl: Duration,
    /// Request limit per connection per minute (`None` = unlimited).
    pub max_requests_per_minute: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct OidcConfig {
    /// Issuer URL, discovered via `/.well-known/openid-configuration`.
    pub issuer_url: String,
    /// Expected `aud` in ID tokens.
    pub client_id: String,
    /// Maximum age (`iat`) of an ID token for sensitive operations
    /// (`auth.enroll`, `devices.revoke`).
    pub max_fresh_token_age: Duration,
    /// Shortest delay between two JWKS fetches. The issuer's signing keys are
    /// re-fetched when a token carries a key id absent from the cache (IdP key
    /// rotation), but no more often than this — otherwise tokens bearing
    /// unknown key ids could each trigger a request to the issuer.
    pub jwks_refresh_min_interval: Duration,
}

pub struct ServerHandle {
    local_addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl ServerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Waits for the server task to finish. Under nominal operation it never
    /// terminates on its own (`axum::serve` loops as long as the socket lives):
    /// this wait therefore returns only if the server stops on error — enough,
    /// for a binary, to tell a requested shutdown from a crashed server.
    pub async fn wait(&mut self) {
        let _ = (&mut self.task).await;
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the server in EPHEMERAL mode: the directory does not survive the
/// process stopping. Handy for tests and experimentation; a deployment wants
/// `spawn_with_store`.
pub async fn spawn(config: Config) -> anyhow::Result<ServerHandle> {
    spawn_with_store(config, Arc::new(MemoryStore::default())).await
}

/// Starts the server with a persistence store. The durable state (device
/// identities, C7 attestations, revocations) is loaded at startup and rewritten
/// after every durable mutation; a load failure prevents startup (an unreadable
/// directory must not be silently restarted from scratch).
/// Returns once the socket is listening.
pub async fn spawn_with_store(
    config: Config,
    store: Arc<dyn DirectoryStore>,
) -> anyhow::Result<ServerHandle> {
    let initial = store.load().context("loading the persisted directory")?;
    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    let local_addr = listener.local_addr()?;
    let app_state = Arc::new(AppState::new(config, store, initial));

    let app = axum::Router::new()
        .route("/health", get(async || "ok"))
        .route("/ws", get(ws_upgrade))
        .with_state(app_state);

    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("server stopped on error: {e}");
        }
    });

    Ok(ServerHandle { local_addr, task })
}

/// The control plane only carries short JSON-RPC messages: capping the frame
/// size prevents an unauthenticated client from making us allocate and parse
/// megabytes (tungstenite's default is 64 MiB).
const MAX_FRAME_BYTES: usize = 256 * 1024;

async fn ws_upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| conn::run(state, socket))
}
