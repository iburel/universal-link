// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Test harness: server started in-process + shared building blocks
//! (fake OIDC, device keys, JSON-RPC WebSocket client — crate
//! `universallink-test-support`).
//!
//! Protocol decisions **frozen by this suite** (complementing doc/server-api.md):
//! - Endpoints: WebSocket on `/ws`, health on `GET /health`. TLS is terminated
//!   upstream — the tests speak in the clear over localhost.
//! - JSON-RPC 2.0: client→server requests with a numeric `id`; server→client
//!   notifications without an `id`; application codes in `error.data.code`
//!   (the standard JSON-RPC codes stay in `error.code`).
//! - Encodings: `node_id` = Ed25519 public key in hex (64 chars);
//!   `proof` = Ed25519 signature in hex (128 chars) of the nonce's UTF-8 bytes;
//!   `nonce` = opaque string, single-use, bound to the connection that requested it.
//! - `api_version` = 1 (integer).
//! - `device_id` prefixed with `d_`.
//! - Notifications are broadcast to all the account's connected devices EXCEPT
//!   the connection that caused the change (the requester has the response).
//! - `auth.enroll` does NOT authenticate the connection: `auth.authenticate` is
//!   then needed to bind the connection to the device (→ online).
//! - Revocation: the revoked device's connection is closed with a close frame
//!   whose reason is `DEVICE_REVOKED`.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub use universallink_test_support::*;

use universallink_server::{
    Config, DirectoryStore, OidcConfig, ServerHandle, spawn, spawn_with_store,
};

// ---------------------------------------------------------------------------
// Test environment: fake OIDC + server.
// ---------------------------------------------------------------------------

pub struct TestEnv {
    pub oidc: FakeOidc,
    pub server: ServerHandle,
}

/// Standard test config, backed by a given fake OIDC.
fn base_config(oidc: &FakeOidc) -> Config {
    Config {
        bind_addr: "127.0.0.1:0".parse().expect("addr"),
        oidc: OidcConfig {
            issuer_url: oidc.issuer(),
            client_id: TEST_CLIENT_ID.into(),
            max_fresh_token_age: Duration::from_secs(300),
        },
        heartbeat_interval: Duration::from_secs(30),
        heartbeat_max_missed: 2,
        nonce_ttl: Duration::from_secs(60),
        max_requests_per_minute: None,
    }
}

impl TestEnv {
    pub async fn start() -> TestEnv {
        TestEnv::start_with(|_| {}).await
    }

    /// Starts with an adjusted config (short heartbeat, short nonce TTL…).
    pub async fn start_with(tweak: impl FnOnce(&mut Config)) -> TestEnv {
        let oidc = FakeOidc::start().await;
        let mut config = base_config(&oidc);
        tweak(&mut config);
        let server = spawn(config).await.expect("server startup");
        TestEnv { oidc, server }
    }

    /// Starts with a given persistence store — to test the directory surviving a
    /// restart (the same store shared between two successive servers). Since
    /// `auth.authenticate` is OIDC-free, a new `FakeOidc` for the second server
    /// is inconsequential.
    pub async fn start_with_store(store: Arc<dyn DirectoryStore>) -> TestEnv {
        let oidc = FakeOidc::start().await;
        let server = spawn_with_store(base_config(&oidc), store)
            .await
            .expect("server startup");
        TestEnv { oidc, server }
    }

    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.server.local_addr())
    }

    pub async fn connect(&self) -> TestConn {
        TestConn::connect(&self.ws_url()).await
    }

    /// Raw HTTP GET; returns the status code.
    pub async fn http_get_status(&self, path: &str) -> u16 {
        let mut stream = TcpStream::connect(self.server.local_addr())
            .await
            .expect("HTTP connection");
        stream
            .write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .expect("HTTP request");
        let mut buf = String::new();
        stream.read_to_string(&mut buf).await.expect("HTTP response");
        buf.split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("HTTP status")
    }
}

// ---------------------------------------------------------------------------
// Environment-bound flows (the generic building blocks come from the crate).
// ---------------------------------------------------------------------------

/// Connects + enrolls a device under `sub`. The connection is NOT authenticated
/// on return (`authenticate` is still needed).
pub async fn enroll_device(env: &TestEnv, sub: &str, name: &str, platform: &str) -> Device {
    enroll_device_at(&env.ws_url(), &env.oidc, sub, name, platform).await
}

/// Enrolls then authenticates on the same connection → device online.
pub async fn online_device(env: &TestEnv, sub: &str, name: &str, platform: &str) -> Device {
    let mut device = enroll_device(env, sub, name, platform).await;
    authenticate(&mut device.conn, &device.key, &device.device_id).await;
    device
}

/// A new authenticated connection for an already-enrolled device.
pub async fn reconnect(env: &TestEnv, device: &Device) -> TestConn {
    let mut conn = env.connect().await;
    authenticate(&mut conn, &device.key, &device.device_id).await;
    conn
}
