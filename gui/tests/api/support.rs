// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Harness: the Tauri shell on MockRuntime, wired to a REAL Core (the
//! universallink-core lib) — same philosophy as the client crate's suite.
//! TestCore/RawComponent/TestServer are a deliberate and REDUCED copy of the
//! client's harness (`client/tests/api/support.rs`, 2nd copy) — to be
//! extracted into test-support if a third copy threatens (tray building block).
//!
//! Shell contract **pinned by this suite**:
//! - `shell(builder, config)` attaches the bridge to the Tauri builder (state +
//!   commands + setup); the IPC client starts at app construction.
//! - Command `connection_status {}` → snapshot `{ status, granted_scopes?,
//!   api_version? }`, status ∈ "connecting" | "connected" | "incompatible".
//!   Initial state: "connecting". A loss of connection BRINGS it back to
//!   "connecting" (reconnection is automatic; fail-closed, up to the frontend
//!   to display the unavailability). "incompatible" is terminal. The snapshot
//!   is updated BEFORE the event is emitted: a frontend that subscribes THEN
//!   reads the snapshot never misses a state.
//! - Command `core_request { method, params? }` → full JSON-RPC proxy to the
//!   Core. There is NOT one command per method: the Core is the sole authority
//!   (validation, scopes); a method added to the Core is available without
//!   touching the shell. `params` omitted = `{}`.
//!   Ok = result as-is. Err = `{ kind: "not_connected" | "timeout" |
//!   "disconnected" | "rpc", message, code?, data_code? }` — faithful relay of
//!   RequestError; when offline the failure is immediate (fail-closed),
//!   including before the first connection is established.
//! - Webview events: "core:connection" (payload = snapshot) on every state
//!   change; "core:notification" (`{ method, params }`) for each Core
//!   notification, as-is, in order. No initial event: the snapshot is
//!   authoritative.
//! - The production config (scopes/topics `GUI_SCOPES`/`GUI_TOPICS`, default
//!   paths `paths::production_endpoint`) is held by the binary; the lib takes
//!   an arbitrary `ClientConfig`.
//!
//! Mechanics: `get_ipc_response` blocks its thread (internal block_on) — the
//! invokes go through `spawn_blocking`, and the tests run in multi_thread
//! flavor with margin to spare. The bridges outlive their test (the AppHandle
//! ↔ state cycle keeps the loop alive): this is intended, the backoff renders
//! them inert and the test process carries them off.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tauri::Listener;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::time::timeout;

use universallink_core::CoreHandle;
use universallink_ipc_client::{ClientConfig, TokenSource};
pub use universallink_test_support::{FakeOidc, TEST_CLIENT_ID, TEST_EMAIL, browse};

pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Observation window to assert that no event arrives.
pub const SILENCE_WINDOW: Duration = Duration::from_millis(300);

/// Name of the Core's device in the directory.
pub const CORE_DEVICE_NAME: &str = "PC-Core";

// ---------------------------------------------------------------------------
// IPC paths per platform.
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
        r"\\.\pipe\universallink-gui-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

// ---------------------------------------------------------------------------
// The real Core in a temporary folder — IPC path stable across restarts,
// token regenerated on each one (a component's contract).
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

    /// Stops the Core (socket closed, orphaned token on disk).
    pub fn stop(&mut self) {
        self.handle = None;
    }

    /// (Re)starts the Core on the same folder and the same IPC path.
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
            // The lib only speaks in the clear: it's the daemon that wires up TLS.
            connector: Arc::new(universallink_core::PlainConnector),
            // These tests don't exercise the data plane: isolated in-memory transport.
            transport: universallink_test_support::memory_transport::MemorySwitchboard::new()
                .endpoint("gui-test", None),
            receive_dir: self.dir.path().join("received"),
            reconnect_base_delay: Duration::from_millis(50),
        };
        // Windows: rebinding the same pipe name may fail for a few ms after
        // the previous Core stops (asynchronous teardown). We keep trying.
        let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
        let handle = loop {
            match universallink_core::spawn(config.clone()).await {
                Ok(handle) => break handle,
                Err(e) => {
                    assert!(tokio::time::Instant::now() < deadline, "Core startup: {e}");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        };
        self.handle = Some(handle);
    }

    pub fn ipc_path(&self) -> PathBuf {
        self.ipc_path.clone()
    }

    pub fn token_path(&self) -> PathBuf {
        self.dir.path().join("ipc-token")
    }

    /// Raw connection (hand-rolled framing) to play a third-party component.
    pub async fn connect_raw(&self) -> RawComponent {
        RawComponent::connect(&self.ipc_path).await
    }
}

/// Production-like GUI config, pointed at a TestCore.
pub fn gui_config(core: &TestCore) -> ClientConfig {
    ClientConfig {
        ipc_path: core.ipc_path(),
        token: TokenSource::File(core.token_path()),
        name: "gui-test".into(),
        version: "0.0-test".into(),
        role: "gui".into(),
        scopes: universallink_gui::GUI_SCOPES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        topics: universallink_gui::GUI_TOPICS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        served_methods: vec![],
        reconnect_base_delay: Duration::from_millis(25),
        request_timeout: RESPONSE_TIMEOUT,
    }
}

// ---------------------------------------------------------------------------
// The shell on MockRuntime: app built by `shell()`, mock webview, invokes via
// Tauri's test IPC, events captured by listen_any.
// ---------------------------------------------------------------------------

type MockRuntime = tauri::test::MockRuntime;

pub struct Shell {
    pub app: tauri::App<MockRuntime>,
    webview: tauri::WebviewWindow<MockRuntime>,
    events: mpsc::UnboundedReceiver<(String, Value)>,
    /// The Core's config directory handed to `shell()`: kept alive here, and
    /// where `set_server_config` writes `config.json` (inspectable by tests).
    config_dir: tempfile::TempDir,
}

/// Builds the app (the bridge starts at construction), wires up the listeners
/// then creates the webview. A state reached BEFORE this function returns may
/// have been emitted before the listeners: synchronize via `wait_status`, not
/// via an event, when the Core is already running.
pub async fn shell_app(config: ClientConfig) -> Shell {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let app = universallink_gui::shell(
        tauri::test::mock_builder(),
        config,
        config_dir.path().to_path_buf(),
    )
    .build(tauri::test::mock_context(tauri::test::noop_assets()))
    .expect("mock app construction");
    let (tx, events) = mpsc::unbounded_channel();
    for name in ["core:connection", "core:notification"] {
        let tx = tx.clone();
        app.listen_any(name, move |event| {
            let payload: Value =
                serde_json::from_str(event.payload()).expect("JSON payload of an event");
            let _ = tx.send((name.to_string(), payload));
        });
    }
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("webview mock");
    Shell {
        app,
        webview,
        events,
        config_dir,
    }
}

impl Shell {
    /// Raw invoke of a Tauri command, as the frontend would do.
    pub async fn invoke(&self, cmd: &str, args: Value) -> Result<Value, Value> {
        let webview = self.webview.clone();
        let cmd = cmd.to_string();
        tokio::task::spawn_blocking(move || {
            let url = if cfg!(windows) {
                "http://tauri.localhost"
            } else {
                "tauri://localhost"
            };
            tauri::test::get_ipc_response(
                &webview,
                tauri::webview::InvokeRequest {
                    cmd,
                    callback: tauri::ipc::CallbackFn(0),
                    error: tauri::ipc::CallbackFn(1),
                    url: url.parse().expect("invoke url"),
                    body: tauri::ipc::InvokeBody::Json(args),
                    headers: Default::default(),
                    invoke_key: tauri::test::INVOKE_KEY.to_string(),
                },
            )
            .map(|body| {
                body.deserialize::<Value>()
                    .expect("command response as JSON")
            })
        })
        .await
        .expect("invoke task")
    }

    pub async fn core_request(&self, method: &str, params: Value) -> Result<Value, Value> {
        self.invoke(
            "core_request",
            json!({ "method": method, "params": params }),
        )
        .await
    }

    pub async fn connection_status(&self) -> Value {
        self.invoke("connection_status", json!({}))
            .await
            .expect("connection_status")
    }

    /// The config directory `set_server_config` writes into.
    pub fn config_dir(&self) -> &std::path::Path {
        self.config_dir.path()
    }

    /// Invokes `set_server_config` (the setup screen's save).
    pub async fn set_server_config(&self, config: Value) -> Result<Value, Value> {
        self.invoke("set_server_config", json!({ "config": config }))
            .await
    }

    /// Invokes `get_server_config` (the setup screen's pre-fill).
    pub async fn get_server_config(&self) -> Value {
        self.invoke("get_server_config", json!({}))
            .await
            .expect("get_server_config")
    }

    /// Waits (by polling the snapshot) for the connection to reach `status` —
    /// the safe synchronization when the event may have preceded the listeners.
    pub async fn wait_status(&self, status: &str) {
        let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let s = self.connection_status().await;
            if s["status"] == json!(status) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "status {status} never reached, last: {s}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Next webview event (core:connection / core:notification).
    pub async fn next_event(&mut self) -> (String, Value) {
        timeout(RESPONSE_TIMEOUT, self.events.recv())
            .await
            .expect("timeout waiting for a webview event")
            .expect("webview event channel closed")
    }

    /// The next event MUST be core:connection with this status; returns the
    /// emitted snapshot.
    pub async fn expect_connection(&mut self, status: &str) -> Value {
        let (name, payload) = self.next_event().await;
        assert_eq!(name, "core:connection", "unexpected event: {payload}");
        assert_eq!(payload["status"], json!(status), "{payload}");
        payload
    }

    /// Waits for a `method` notification, ignoring other notifications. The
    /// only connection event tolerated during the wait: the initial
    /// "connected" (listeners/bridge race at startup) — any other state change
    /// is an error, the connection must not move during traffic.
    pub async fn wait_core_notification(&mut self, method: &str) -> Value {
        loop {
            let (name, payload) = self.next_event().await;
            if name == "core:connection" {
                assert_eq!(
                    payload["status"],
                    json!("connected"),
                    "unexpected connection state while waiting for {method}: {payload}"
                );
                continue;
            }
            if payload["method"] == json!(method) {
                return payload["params"].clone();
            }
        }
    }

    /// Checks that no webview event arrives during `SILENCE_WINDOW`.
    pub async fn assert_no_event(&mut self) {
        match timeout(SILENCE_WINDOW, self.events.recv()).await {
            Err(_) => {}
            Ok(Some(e)) => panic!("unexpected webview event: {e:?}"),
            Ok(None) => panic!("event channel closed during the silence window"),
        }
    }
}

// ---------------------------------------------------------------------------
// Raw component: hand-rolled LSP framing + JSON-RPC — plays the third-party
// component whose enrollment solicits the GUI.
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
            Err(e) => panic!("pipe connection: {e}"),
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
        let bytes = frame(&msg.to_string());
        self.writer.write_all(&bytes).await.expect("IPC write");
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

    /// full hello; panics if the response has no result.
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

    async fn recv_json(&mut self) -> Value {
        let text = recv_frame(&mut self.reader)
            .await
            .expect("connection closed by the Core");
        serde_json::from_str(&text).expect("invalid JSON received from the Core")
    }
}

/// Encodes `text` into an LSP frame.
fn frame(text: &str) -> Vec<u8> {
    let mut bytes = format!("Content-Length: {}\r\n\r\n", text.len()).into_bytes();
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// Next LSP frame; `None` = EOF (or an assimilated reset).
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
    Some(String::from_utf8(buf).expect("non-UTF-8 payload"))
}

// ---------------------------------------------------------------------------
// The real server + FakeOidc for the full-stack module.
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
}
