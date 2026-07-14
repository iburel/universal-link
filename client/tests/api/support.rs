// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Test harness: the client crate consumed against a REAL Core
//! (universallink-core lib) started in a temporary directory — same
//! philosophy as the Core's suite against the real server lib. A "scripted
//! Core" (minimal IPC server driven by the test) rounds it out for the
//! behaviors the real Core cannot trigger (foreign api_version, response
//! never returned, invalid frame, incoming request).
//!
//! Crate contract **frozen by this suite** (complements doc/core-api.md):
//! - Managed cycle: connection → hello (the token is re-read from disk on
//!   EVERY attempt for `TokenSource::File` — the Core regenerates `ipc-token`
//!   on each startup) → `events.subscribe(topics)` if `topics` is non-empty →
//!   `Event::Connected { granted_scopes, api_version }`. Notifications
//!   received during establishment are delivered after `Connected`, in
//!   order.
//! - Any cycle failure (connection refused, unreadable token, hello or
//!   subscribe in error, framing violation, EOF) → `Event::Disconnected`
//!   if a connection had been established, then retries with exponential
//!   backoff (base `ClientConfig.reconnect_base_delay`, doubled, capped at
//!   60 s, reset to the base after a successful establishment). A
//!   deterministic config error (invalid token, missing scope for a topic)
//!   thus loops forever: the symptom is "never Connected", for the consumer
//!   to display it (fail-closed).
//! - Exception: `api_version` ≠ 1 → `Event::Incompatible`, PERMANENT shutdown
//!   of the client (no reconnection; any subsequent request →
//!   `NotConnected`). An incompatibility does not heal by retrying.
//! - Requests: multiplexed (increasing ids, never reused), timeout
//!   `ClientConfig.request_timeout` → `RequestError::Timeout` — it also covers
//!   enqueuing the command (a suspended manager, for example under event
//!   backpressure, never blocks the caller without bound); offline →
//!   immediate `NotConnected`, INCLUDING during an establishment attempt
//!   (no offline queue that would replay on the fresh connection —
//!   fail-closed); connection lost during the request →
//!   `RequestError::Disconnected`. JSON-RPC errors are relayed as-is
//!   (`code`, `message`, `data.code`).
//! - Notifications: relayed as-is (`{ method, params }`), in the order
//!   received from the socket. No ordering guarantee between a response and a
//!   notification (distinct channels).
//! - Incoming Core→component request: `-32601` (the v1 client serves no
//!   method); the connection survives.
//! - `hello` answered `{ status: "pending" }` (interactive third-party
//!   enrollment): not supported in v1 — treated as a cycle failure (retry).
//!   The approval flow will come with the third-party components SDK.
//! - Resolution of the production default paths (socket, config_dir):
//!   out of scope — the GUI building block will carry it.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::time::timeout;

use universallink_core::CoreHandle;
use universallink_ipc_client::{Client, ClientConfig, Event, TokenSource};
pub use universallink_test_support::{
    Device, FakeOidc, TEST_CLIENT_ID, TEST_EMAIL, TEST_SUB, authenticate, browse, enroll_device_at,
};

pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Observation window to assert that no event arrives.
pub const SILENCE_WINDOW: Duration = Duration::from_millis(300);

/// The Core's device name in the directory (same value as the Core's suite).
pub const CORE_DEVICE_NAME: &str = "PC-Core";

// ---------------------------------------------------------------------------
// Per-platform IPC paths (same conventions as the Core's suite).
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn ipc_path_for(dir: &Path) -> PathBuf {
    dir.join("core.sock")
}

#[cfg(windows)]
fn ipc_path_for(_dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    PathBuf::from(format!(
        r"\\.\pipe\universallink-client-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

// ---------------------------------------------------------------------------
// The real Core in a temporary directory. Unlike the Core's suite,
// the IPC path is allocated ONCE and survives restarts: that is the
// contract of a component (the path is stable, the token is not).
// ---------------------------------------------------------------------------

pub struct TestCore {
    handle: Option<CoreHandle>,
    dir: tempfile::TempDir,
    ipc_path: PathBuf,
    server_cfg: Option<universallink_core::ServerConfig>,
}

impl TestCore {
    pub async fn start() -> TestCore {
        Self::spawn_in(None).await
    }

    /// Core configured (server + OIDC) but never logged in.
    pub async fn start_with_server(server: &TestServer) -> TestCore {
        Self::spawn_in(Some(server.core_cfg())).await
    }

    async fn spawn_in(server_cfg: Option<universallink_core::ServerConfig>) -> TestCore {
        let dir = tempfile::tempdir().expect("tempdir");
        let ipc_path = ipc_path_for(dir.path());
        let mut core = TestCore {
            handle: None,
            dir,
            ipc_path,
            server_cfg,
        };
        core.restart().await;
        core
    }

    /// Stops the Core (socket closed, orphan token on disk).
    pub fn stop(&mut self) {
        self.handle = None;
    }

    /// (Re)starts the Core on the same directory and the same IPC path — the
    /// file token is regenerated, as on every real startup.
    pub async fn restart(&mut self) {
        self.handle = None;
        let config = universallink_core::Config {
            ipc_path: self.ipc_path.clone(),
            config_dir: self.dir.path().to_path_buf(),
            server: self.server_cfg.clone(),
            reload_server: {
                let s = self.server_cfg.clone();
                Arc::new(move || Ok::<_, String>(s.clone()))
            },
            device_name: CORE_DEVICE_NAME.into(),
            secret_store: Arc::new(universallink_core::FileSecretStore::new(self.dir.path())),
            // The lib speaks only in cleartext: it is the daemon that wires up TLS.
            connector: Arc::new(universallink_core::PlainConnector),
            // These tests do not exercise the data plane: isolated in-memory transport.
            transport: universallink_test_support::memory_transport::MemorySwitchboard::new()
                .endpoint("client-test", None),
            receive_dir: self.dir.path().join("received"),
            reconnect_base_delay: Duration::from_millis(50),
        };
        // Windows: the previous Core's pipe instance disappears along with its
        // accept task, dropped asynchronously — rebinding the same name
        // (first_pipe_instance) may fail for a few ms. We keep trying.
        let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
        let handle = loop {
            match universallink_core::spawn(config.clone()).await {
                Ok(handle) => break handle,
                Err(e) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "Core startup: {e}"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        };
        self.handle = Some(handle);
    }

    pub fn ipc_path(&self) -> PathBuf {
        self.ipc_path.clone()
    }

    /// File token path — the client's `TokenSource::File` source.
    pub fn token_path(&self) -> PathBuf {
        self.dir.path().join("ipc-token")
    }

    pub fn file_token(&self) -> String {
        std::fs::read_to_string(self.token_path())
            .expect("reading ipc-token")
            .trim()
            .to_string()
    }

    /// Bootstrap path B: spawn token minted by the Core.
    pub fn mint(&self, role: &str, scopes: &[&str]) -> String {
        self.handle
            .as_ref()
            .expect("Core stopped")
            .mint_spawn_token(role, scopes)
    }

    /// Raw connection (without the client crate) to trigger events on the
    /// Core side — third-party component pending, etc.
    pub async fn connect_raw(&self) -> RawComponent {
        RawComponent::connect(&self.ipc_path).await
    }
}

// ---------------------------------------------------------------------------
// Config and helpers for the client crate.
// ---------------------------------------------------------------------------

pub fn client_config(
    core: &TestCore,
    role: &str,
    scopes: &[&str],
    topics: &[&str],
) -> ClientConfig {
    ClientConfig {
        ipc_path: core.ipc_path(),
        token: TokenSource::File(core.token_path()),
        name: "client-test".into(),
        version: "0.0-test".into(),
        role: role.into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        topics: topics.iter().map(|s| s.to_string()).collect(),
        reconnect_base_delay: Duration::from_millis(25),
        request_timeout: RESPONSE_TIMEOUT,
    }
}

/// Client started and connected; checks the granted scopes and the api_version.
pub async fn connected(
    core: &TestCore,
    role: &str,
    scopes: &[&str],
    topics: &[&str],
) -> (Client, mpsc::Receiver<Event>) {
    let (client, mut events) =
        universallink_ipc_client::spawn(client_config(core, role, scopes, topics));
    expect_connected(&mut events, scopes).await;
    (client, events)
}

pub async fn next_event(events: &mut mpsc::Receiver<Event>) -> Event {
    timeout(RESPONSE_TIMEOUT, events.recv())
        .await
        .expect("timeout waiting for an event")
        .expect("event channel closed")
}

pub async fn expect_connected(events: &mut mpsc::Receiver<Event>, scopes: &[&str]) {
    match next_event(events).await {
        Event::Connected {
            granted_scopes,
            api_version,
        } => {
            assert_eq!(granted_scopes, scopes, "granted scopes");
            assert_eq!(api_version, 1, "api_version");
        }
        other => panic!("unexpected event while waiting for Connected: {other:?}"),
    }
}

pub async fn expect_disconnected(events: &mut mpsc::Receiver<Event>) {
    match next_event(events).await {
        Event::Disconnected => {}
        other => panic!("unexpected event while waiting for Disconnected: {other:?}"),
    }
}

/// The next event MUST be a notification; returns it.
pub async fn expect_notification(events: &mut mpsc::Receiver<Event>) -> (String, Value) {
    match next_event(events).await {
        Event::Notification { method, params } => (method, params),
        other => panic!("unexpected event while waiting for a notification: {other:?}"),
    }
}

/// Waits for a `method` notification, ignoring the OTHER notifications
/// (but any connection event is an error).
pub async fn wait_notification(events: &mut mpsc::Receiver<Event>, method: &str) -> Value {
    loop {
        let (m, params) = expect_notification(events).await;
        if m == method {
            return params;
        }
    }
}

/// Checks that no event arrives during `SILENCE_WINDOW`.
pub async fn assert_no_event(events: &mut mpsc::Receiver<Event>) {
    match timeout(SILENCE_WINDOW, events.recv()).await {
        Err(_) => {}
        Ok(Some(e)) => panic!("unexpected event: {e:?}"),
        Ok(None) => panic!("event channel closed during the silence window"),
    }
}

// ---------------------------------------------------------------------------
// Raw component: LSP framing + JSON-RPC by hand, independent of the client
// crate (reduced copy of the Core's harness) — used to trigger events
// and to observe the protocol from "the other side".
// ---------------------------------------------------------------------------

#[cfg(unix)]
type ClientStream = tokio::net::UnixStream;
#[cfg(windows)]
type ClientStream = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(unix)]
async fn connect_stream(path: &Path) -> ClientStream {
    tokio::net::UnixStream::connect(path)
        .await
        .expect("UDS connection")
}

#[cfg(windows)]
async fn connect_stream(path: &Path) -> ClientStream {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    let name = path.to_str().expect("UTF-8 pipe name");
    loop {
        match ClientOptions::new().open(name) {
            Ok(stream) => return stream,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("connection to the pipe: {e}"),
        }
    }
}

pub struct RawComponent {
    reader: BufReader<ReadHalf<ClientStream>>,
    writer: WriteHalf<ClientStream>,
    next_id: u64,
    notifications: VecDeque<(String, Value)>,
}

impl RawComponent {
    async fn connect(path: &Path) -> RawComponent {
        let stream = connect_stream(path).await;
        let (read, write) = tokio::io::split(stream);
        RawComponent {
            reader: BufReader::new(read),
            writer: write,
            next_id: 0,
            notifications: VecDeque::new(),
        }
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send_frame(&msg.to_string()).await;
        timeout(RESPONSE_TIMEOUT, async {
            loop {
                let v = self.recv_json().await;
                if v.get("method").is_some() {
                    let method = v["method"].as_str().expect("method").to_string();
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    self.notifications.push_back((method, params));
                } else {
                    assert_eq!(v.get("id"), Some(&json!(id)), "response for another id");
                    return v;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timeout waiting for the response to {method}"))
    }

    /// Full hello; panics if the result is not the expected one.
    pub async fn hello(
        &mut self,
        name: &str,
        role: &str,
        scopes: &[&str],
        token: Option<&str>,
    ) -> Value {
        let mut params = json!({
            "name": name,
            "version": "0.0-test",
            "role": role,
            "scopes": scopes,
        });
        if let Some(token) = token {
            params["token"] = json!(token);
        }
        let v = self.request("hello", params).await;
        v.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("hello in error: {v}"))
    }

    pub async fn expect_notification(&mut self, method: &str) -> Value {
        if let Some(pos) = self.notifications.iter().position(|(m, _)| m == method) {
            return self.notifications.remove(pos).expect("notification").1;
        }
        timeout(RESPONSE_TIMEOUT, async {
            loop {
                let v = self.recv_json().await;
                let m = v["method"]
                    .as_str()
                    .unwrap_or_else(|| panic!("unexpected response: {v}"));
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                if m == method {
                    return params;
                }
                self.notifications.push_back((m.to_string(), params));
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timeout waiting for {method}"))
    }

    async fn send_frame(&mut self, text: &str) {
        let bytes = frame(text);
        self.writer.write_all(&bytes).await.expect("IPC write");
    }

    async fn recv_json(&mut self) -> Value {
        let text = recv_frame(&mut self.reader)
            .await
            .expect("connection closed by the Core");
        serde_json::from_str(&text).expect("invalid JSON received from the Core")
    }
}

/// Encodes `text` into an LSP frame.
pub fn frame(text: &str) -> Vec<u8> {
    let mut bytes = format!("Content-Length: {}\r\n\r\n", text.len()).into_bytes();
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// Next LSP frame read from `reader`; `None` = EOF (or equivalent reset).
async fn recv_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Option<String> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = match reader.read_line(&mut line).await {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                0
            }
            Err(e) => panic!("IPC read: {e}"),
        };
        if n == 0 {
            assert!(content_length.is_none(), "EOF in the middle of the headers");
            return None;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        let (name, value) = trimmed
            .split_once(':')
            .unwrap_or_else(|| panic!("header without a colon: {trimmed:?}"));
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.trim().parse().expect("unreadable Content-Length"));
        }
    }
    let len = content_length.expect("frame without Content-Length");
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.expect("IPC payload");
    Some(String::from_utf8(buf).expect("payload not UTF-8"))
}

// ---------------------------------------------------------------------------
// Scripted Core: a minimal IPC server whose every exchange is driven by the
// test — for the behaviors the real Core cannot produce.
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub struct ScriptedCore {
    _dir: tempfile::TempDir,
    path: PathBuf,
    listener: tokio::net::UnixListener,
}

#[cfg(unix)]
impl ScriptedCore {
    pub async fn start() -> ScriptedCore {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scripted.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("scripted bind");
        ScriptedCore {
            _dir: dir,
            path,
            listener,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub async fn accept(&mut self) -> ScriptedConn {
        let (stream, _) = timeout(RESPONSE_TIMEOUT, self.listener.accept())
            .await
            .expect("timeout waiting for a connection")
            .expect("scripted accept");
        ScriptedConn::new(stream)
    }

    /// Checks that no connection arrives during `SILENCE_WINDOW`.
    pub async fn assert_no_connection(&mut self) {
        if timeout(SILENCE_WINDOW, self.listener.accept())
            .await
            .is_ok()
        {
            panic!("unexpected connection to the scripted Core");
        }
    }
}

#[cfg(unix)]
type ScriptedStream = tokio::net::UnixStream;

#[cfg(windows)]
pub struct ScriptedCore {
    path: PathBuf,
    /// Instance waiting for the next client (created before yielding the
    /// previous one, like the real transport).
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
impl ScriptedCore {
    pub async fn start() -> ScriptedCore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from(format!(
            r"\\.\pipe\universallink-scripted-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let next = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&path)
            .expect("scripted pipe creation");
        ScriptedCore { path, next }
    }

    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub async fn accept(&mut self) -> ScriptedConn {
        timeout(RESPONSE_TIMEOUT, self.next.connect())
            .await
            .expect("timeout waiting for a connection")
            .expect("scripted connect");
        let replacement = tokio::net::windows::named_pipe::ServerOptions::new()
            .create(&self.path)
            .expect("next scripted pipe instance");
        let stream = std::mem::replace(&mut self.next, replacement);
        ScriptedConn::new(stream)
    }

    pub async fn assert_no_connection(&mut self) {
        if timeout(SILENCE_WINDOW, self.next.connect()).await.is_ok() {
            panic!("unexpected connection to the scripted Core");
        }
    }
}

#[cfg(windows)]
type ScriptedStream = tokio::net::windows::named_pipe::NamedPipeServer;

pub struct ScriptedConn {
    reader: BufReader<ReadHalf<ScriptedStream>>,
    writer: WriteHalf<ScriptedStream>,
}

impl ScriptedConn {
    fn new(stream: ScriptedStream) -> ScriptedConn {
        let (read, write) = tokio::io::split(stream);
        ScriptedConn {
            reader: BufReader::new(read),
            writer: write,
        }
    }

    /// Next JSON message received from the client.
    pub async fn recv(&mut self) -> Value {
        let text = timeout(RESPONSE_TIMEOUT, recv_frame(&mut self.reader))
            .await
            .expect("timeout waiting for a frame from the client")
            .expect("connection closed by the client");
        serde_json::from_str(&text).expect("invalid JSON received from the client")
    }

    /// Awaits the client closing the connection.
    pub async fn expect_close(&mut self) {
        let r = timeout(RESPONSE_TIMEOUT, recv_frame(&mut self.reader))
            .await
            .expect("timeout waiting for the client to close");
        assert!(r.is_none(), "unexpected frame before close: {r:?}");
    }

    pub async fn send(&mut self, v: &Value) {
        let bytes = frame(&v.to_string());
        self.writer
            .write_all(&bytes)
            .await
            .expect("scripted write");
    }

    pub async fn send_raw(&mut self, bytes: &[u8]) {
        self.writer
            .write_all(bytes)
            .await
            .expect("scripted write");
    }

    /// Reads the client's hello and accepts it: granted scopes = requested,
    /// `api_version` at the test's discretion.
    pub async fn handle_hello(&mut self, api_version: u64) {
        let v = self.recv().await;
        assert_eq!(v["method"], "hello", "first message expected: hello");
        let scopes = v["params"]["scopes"].clone();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": v["id"],
            "result": { "status": "ok", "granted_scopes": scopes, "api_version": api_version },
        }))
        .await;
    }
}

// ---------------------------------------------------------------------------
// The real server + FakeOidc, for the full-stack module (full chain
// GUI → client → Core → server + IdP, all in-process). No severable proxy
// here: losing the server is the Core's problem, not the client's — the
// Core's own suite covers it.
// ---------------------------------------------------------------------------

pub struct TestServer {
    pub oidc: FakeOidc,
    _server: universallink_server::ServerHandle,
    url: String,
}

impl TestServer {
    pub async fn start() -> TestServer {
        let oidc = FakeOidc::start().await;
        let config = universallink_server::Config {
            bind_addr: "127.0.0.1:0".parse().expect("addr"),
            oidc: universallink_server::OidcConfig {
                issuer_url: oidc.issuer(),
                client_id: TEST_CLIENT_ID.into(),
                max_fresh_token_age: Duration::from_secs(300),
            },
            heartbeat_interval: Duration::from_secs(30),
            heartbeat_max_missed: 2,
            nonce_ttl: Duration::from_secs(60),
            max_requests_per_minute: None,
        };
        let server = universallink_server::spawn(config)
            .await
            .expect("server startup");
        let url = format!("ws://{}/ws", server.local_addr());
        TestServer {
            oidc,
            _server: server,
            url,
        }
    }

    /// The config a Core pointed at this environment receives.
    pub fn core_cfg(&self) -> universallink_core::ServerConfig {
        universallink_core::ServerConfig {
            url: self.url.clone(),
            oidc_issuer: self.oidc.issuer(),
            oidc_client_id: TEST_CLIENT_ID.into(),
            oidc_client_secret: None,
        }
    }

    /// Harness device, enrolled and authenticated (online) on the test
    /// account — "another PC" of the same account.
    pub async fn online_device(&self, name: &str, platform: &str) -> Device {
        let mut d = enroll_device_at(&self.url, &self.oidc, TEST_SUB, name, platform).await;
        authenticate(&mut d.conn, &d.key, &d.device_id).await;
        d
    }
}
