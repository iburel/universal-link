// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Test harness for the clipboard orchestrator. Two Cores, same philosophy as
//! the client crate's suite:
//! - a REAL `universallink-core` in a temp directory drives the source side end
//!   to end (announce, supersession, the provider-channel serve of an inline
//!   paste) — a second client (role `custom`, `clipboard.read`) plays the
//!   pasting device and discovers the `tx_id` via `clipboard.current`;
//! - a SCRIPTED Core (a minimal IPC server driven by the test) rounds it out for
//!   the destination side and resync, which a lone real Core cannot trigger
//!   (`clipboard.remote_updated` needs a peer).
//!
//! [`FakeBackend`] stands in for a platform backend: it records the downcalls
//! and answers `provide` from a table.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::time::timeout;

use universallink_clipboard::{BackendEvent, ClipboardBackend, RemoteClip, run};
use universallink_core::CoreHandle;
use universallink_ipc_client::{Client, ClientConfig, Event, TokenSource};

pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
pub const SILENCE_WINDOW: Duration = Duration::from_millis(300);
/// How long the assertion helpers poll the fake backend for a downcall.
pub const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
pub const CORE_DEVICE_NAME: &str = "PC-Core";

// ---------------------------------------------------------------------------
// Per-platform IPC paths (same conventions as the Core and client suites).
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
        r"\\.\pipe\universallink-clipboard-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

// ---------------------------------------------------------------------------
// The real Core in a temporary directory.
// ---------------------------------------------------------------------------

pub struct TestCore {
    handle: Option<CoreHandle>,
    dir: tempfile::TempDir,
    ipc_path: PathBuf,
}

impl TestCore {
    pub async fn start() -> TestCore {
        let dir = tempfile::tempdir().expect("tempdir");
        let ipc_path = ipc_path_for(dir.path());
        let config = universallink_core::Config {
            ipc_path: ipc_path.clone(),
            config_dir: dir.path().to_path_buf(),
            server: None,
            reload_server: Arc::new(|| Ok::<_, String>(None)),
            device_name: CORE_DEVICE_NAME.into(),
            secret_store: Arc::new(universallink_core::FileSecretStore::new(dir.path())),
            connector: Arc::new(universallink_core::PlainConnector),
            transport: universallink_test_support::memory_transport::MemorySwitchboard::new()
                .endpoint("clipboard-test", None),
            receive_dir: dir.path().join("received"),
            reconnect_base_delay: Duration::from_millis(50),
        };
        let handle = universallink_core::spawn(config)
            .await
            .expect("Core startup");
        TestCore {
            handle: Some(handle),
            dir,
            ipc_path,
        }
    }

    pub fn ipc_path(&self) -> PathBuf {
        self.ipc_path.clone()
    }

    /// Spawn token minted by the Core for `role`/`scopes`.
    pub fn mint(&self, role: &str, scopes: &[&str]) -> String {
        self.handle
            .as_ref()
            .expect("Core stopped")
            .mint_spawn_token(role, scopes)
    }
}

// ---------------------------------------------------------------------------
// Client helpers.
// ---------------------------------------------------------------------------

pub fn spawn_client(
    ipc_path: &Path,
    token: String,
    role: &str,
    scopes: &[&str],
    served: &[&str],
) -> (Client, mpsc::Receiver<Event>) {
    universallink_ipc_client::spawn(ClientConfig {
        ipc_path: ipc_path.to_path_buf(),
        token: TokenSource::Spawn(token),
        name: "clipboard-test".into(),
        version: "0.0-test".into(),
        role: role.into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        topics: vec![],
        served_methods: served.iter().map(|s| s.to_string()).collect(),
        reconnect_base_delay: Duration::from_millis(25),
        request_timeout: RESPONSE_TIMEOUT,
    })
}

pub async fn expect_connected(events: &mut mpsc::Receiver<Event>) {
    match timeout(RESPONSE_TIMEOUT, events.recv())
        .await
        .expect("timeout waiting for Connected")
        .expect("event channel closed")
    {
        Event::Connected { .. } => {}
        other => panic!("unexpected event while waiting for Connected: {other:?}"),
    }
}

/// The scopes the real clipboard backend holds.
pub const BACKEND_SCOPES: [&str; 3] = ["clipboard.read", "clipboard.write", "devices.read"];

/// A connected clipboard-backend client (the orchestrator's Core side), plus the
/// backend-event sender the test drives.
pub struct Backend {
    pub client: Client,
    pub events: mpsc::Receiver<Event>,
    pub fake: FakeBackend,
    pub backend_tx: mpsc::Sender<BackendEvent>,
    pub backend_rx: Option<mpsc::Receiver<BackendEvent>>,
}

impl Backend {
    /// Connects the orchestrator's client to `core` as the clipboard backend.
    pub async fn connect(core: &TestCore) -> Backend {
        let token = core.mint("clipboard-backend", &BACKEND_SCOPES);
        let (client, mut events) = spawn_client(
            &core.ipc_path(),
            token,
            "clipboard-backend",
            &BACKEND_SCOPES,
            &["clipboard.get_data"],
        );
        expect_connected(&mut events).await;
        let (backend_tx, backend_rx) = mpsc::channel(16);
        Backend {
            client,
            events,
            fake: FakeBackend::default(),
            backend_tx,
            backend_rx: Some(backend_rx),
        }
    }
}

/// A second client that plays the pasting device against the real Core: it may
/// call `clipboard.current`, `transactions.open`, and open consumer channels.
pub struct Consumer {
    pub client: Client,
    ipc_path: PathBuf,
}

impl Consumer {
    pub async fn connect(core: &TestCore) -> Consumer {
        let token = core.mint("custom", &["clipboard.read"]);
        let (client, mut events) =
            spawn_client(&core.ipc_path(), token, "custom", &["clipboard.read"], &[]);
        expect_connected(&mut events).await;
        Consumer {
            client,
            ipc_path: core.ipc_path(),
        }
    }

    pub fn ipc_path(&self) -> &Path {
        &self.ipc_path
    }

    /// Polls `clipboard.current` until it reports a clip whose formats match
    /// `expected_formats` (format ids). Returns its `tx_id`.
    pub async fn await_current_tx(&self, expected_formats: &[&str]) -> String {
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            let current = self
                .client
                .request("clipboard.current", json!({}))
                .await
                .expect("clipboard.current");
            if let Some(tx_id) = current["tx_id"].as_str() {
                let ids: Vec<&str> = current["formats"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f["format"].as_str())
                    .collect();
                if ids == expected_formats {
                    return tx_id.to_string();
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "clipboard.current never reported formats {expected_formats:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Polls `clipboard.current` until it reports a cleared clipboard: either no
    /// clip at all, or a contentless transaction (empty `formats`) — an empty
    /// announce supersedes with a contentless transaction, it does not erase the
    /// snapshot.
    pub async fn await_cleared(&self) {
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            let current = self
                .client
                .request("clipboard.current", json!({}))
                .await
                .expect("clipboard.current");
            let empty = current.get("tx_id").is_none()
                || current["formats"]
                    .as_array()
                    .is_none_or(|formats| formats.is_empty());
            if empty {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "clipboard.current never cleared"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn open(&self, tx_id: &str) -> Result<String, String> {
        match self
            .client
            .request("transactions.open", json!({ "tx_id": tx_id }))
            .await
        {
            Ok(v) => Ok(v["channel_token"]
                .as_str()
                .expect("channel_token")
                .to_string()),
            Err(universallink_ipc_client::RequestError::Rpc(e)) => {
                Err(e.data_code.unwrap_or_else(|| e.message.clone()))
            }
            Err(e) => panic!("transactions.open transport error: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The fake platform backend: records downcalls, answers `provide` from a table.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct FakeBackend {
    inner: Arc<Mutex<FakeInner>>,
}

#[derive(Default)]
struct FakeInner {
    provisions: HashMap<(u64, String), Vec<u8>>,
    offers: Vec<RemoteClip>,
    delivered: Vec<(u64, String, Vec<u8>)>,
    failed: Vec<(u64, String)>,
    releases: usize,
}

impl FakeBackend {
    /// Makes `provide(generation, format)` return `bytes`.
    pub fn provision(&self, generation: u64, format: &str, bytes: &[u8]) {
        self.inner
            .lock()
            .unwrap()
            .provisions
            .insert((generation, format.to_string()), bytes.to_vec());
    }

    pub fn offers(&self) -> Vec<RemoteClip> {
        self.inner.lock().unwrap().offers.clone()
    }

    pub fn delivered(&self) -> Vec<(u64, String, Vec<u8>)> {
        self.inner.lock().unwrap().delivered.clone()
    }

    pub fn failed(&self) -> Vec<(u64, String)> {
        self.inner.lock().unwrap().failed.clone()
    }

    pub fn releases(&self) -> usize {
        self.inner.lock().unwrap().releases
    }

    pub async fn await_offer(&self) -> RemoteClip {
        self.poll_until(|i| i.offers.first().cloned()).await
    }

    pub async fn await_delivered(&self) -> (u64, String, Vec<u8>) {
        self.poll_until(|i| i.delivered.first().cloned()).await
    }

    pub async fn await_failed(&self) -> (u64, String) {
        self.poll_until(|i| i.failed.first().cloned()).await
    }

    pub async fn await_release(&self) {
        self.poll_until(|i| (i.releases > 0).then_some(())).await
    }

    async fn poll_until<T>(&self, mut pick: impl FnMut(&FakeInner) -> Option<T>) -> T {
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            if let Some(value) = pick(&self.inner.lock().unwrap()) {
                return value;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the backend downcall never happened"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl ClipboardBackend for FakeBackend {
    fn provide(
        &self,
        generation: u64,
        format: &str,
    ) -> impl std::future::Future<Output = Option<Vec<u8>>> + Send {
        let out = self
            .inner
            .lock()
            .unwrap()
            .provisions
            .get(&(generation, format.to_string()))
            .cloned();
        async move { out }
    }

    fn offer(&self, clip: RemoteClip) {
        self.inner.lock().unwrap().offers.push(clip);
    }

    fn deliver(&self, token: u64, format: &str, bytes: Vec<u8>) {
        self.inner
            .lock()
            .unwrap()
            .delivered
            .push((token, format.to_string(), bytes));
    }

    fn paste_failed(&self, token: u64, format: &str) {
        self.inner
            .lock()
            .unwrap()
            .failed
            .push((token, format.to_string()));
    }

    fn release(&self) {
        self.inner.lock().unwrap().releases += 1;
    }
}

// ---------------------------------------------------------------------------
// LSP framing (control plane) — a reduced copy of the client's harness.
// ---------------------------------------------------------------------------

pub fn frame(text: &str) -> Vec<u8> {
    let mut bytes = format!("Content-Length: {}\r\n\r\n", text.len()).into_bytes();
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

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
// Scripted Core: a minimal IPC server driven by the test (destination + resync).
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
}

#[cfg(unix)]
type ScriptedStream = tokio::net::UnixStream;

#[cfg(windows)]
pub struct ScriptedCore {
    path: PathBuf,
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
impl ScriptedCore {
    pub async fn start() -> ScriptedCore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from(format!(
            r"\\.\pipe\universallink-clipboard-scripted-{}-{}",
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

    /// Next JSON message (an LSP frame) from the client.
    pub async fn recv(&mut self) -> Value {
        let text = timeout(RESPONSE_TIMEOUT, recv_frame(&mut self.reader))
            .await
            .expect("timeout waiting for a frame from the client")
            .expect("connection closed by the client");
        serde_json::from_str(&text).expect("invalid JSON received from the client")
    }

    pub async fn send(&mut self, v: &Value) {
        let bytes = frame(&v.to_string());
        self.writer.write_all(&bytes).await.expect("scripted write");
    }

    /// Reads the client's hello and accepts it (granted scopes = requested).
    pub async fn handle_hello(&mut self) {
        let v = self.recv().await;
        assert_eq!(v["method"], "hello", "first message expected: hello");
        let scopes = v["params"]["scopes"].clone();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": v["id"],
            "result": { "status": "ok", "granted_scopes": scopes, "api_version": 1 },
        }))
        .await;
    }

    /// Reads one request and replies `result`. Panics unless its method is
    /// `method`. Returns the request's `params`.
    pub async fn handle_request(&mut self, method: &str, result: Value) -> Value {
        let v = self.recv().await;
        assert_eq!(v["method"], method, "unexpected request");
        self.send(&json!({ "jsonrpc": "2.0", "id": v["id"], "result": result }))
            .await;
        v["params"].clone()
    }

    /// Reads one request and replies an application error `code`. Panics unless
    /// its method is `method`.
    pub async fn handle_request_error(&mut self, method: &str, code: &str) -> Value {
        let v = self.recv().await;
        assert_eq!(v["method"], method, "unexpected request");
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": v["id"],
            "error": { "code": -32000, "message": code, "data": { "code": code } },
        }))
        .await;
        v["params"].clone()
    }

    /// Sends a notification (no id).
    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }

    // --- data channel (after the attach frame) ---

    /// Reads the LSP attach frame, returning its `channel_token`.
    pub async fn recv_attach(&mut self) -> String {
        let v = self.recv().await;
        assert!(v.get("method").is_none(), "attach frame carries no method");
        v["channel_token"]
            .as_str()
            .expect("attach channel_token")
            .to_string()
    }

    /// Next binary data-channel frame → `(tag, payload)`.
    pub async fn recv_binary(&mut self) -> (u8, Vec<u8>) {
        let mut len_buf = [0u8; 4];
        timeout(RESPONSE_TIMEOUT, self.reader.read_exact(&mut len_buf))
            .await
            .expect("timeout waiting for a binary frame")
            .expect("binary frame length");
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut frame = vec![0u8; len];
        self.reader
            .read_exact(&mut frame)
            .await
            .expect("binary frame payload");
        (frame[0], frame[1..].to_vec())
    }

    pub async fn send_data(&mut self, offset: u64, bytes: &[u8]) {
        let mut payload = offset.to_be_bytes().to_vec();
        payload.extend_from_slice(bytes);
        self.send_binary(0x10, &payload).await;
    }

    pub async fn send_eof(&mut self) {
        self.send_binary(0x11, &[]).await;
    }

    pub async fn send_channel_error(&mut self, code: &str) {
        let payload = json!({ "code": code }).to_string().into_bytes();
        self.send_binary(0x12, &payload).await;
    }

    async fn send_binary(&mut self, tag: u8, payload: &[u8]) {
        let len = u32::try_from(1 + payload.len()).expect("frame too large");
        let mut frame = len.to_be_bytes().to_vec();
        frame.push(tag);
        frame.extend_from_slice(payload);
        self.writer
            .write_all(&frame)
            .await
            .expect("binary frame write");
    }
}

/// A `stdin_closed` future that never resolves — the orchestrator loop is torn
/// down by ending its event/backend streams instead.
pub fn never() -> impl std::future::Future<Output = ()> {
    std::future::pending()
}

/// Boots the orchestrator against a fresh scripted Core, walks the connect
/// handshake, and answers the resync `clipboard.current` with "no live clip".
/// Returns the live control connection, the backend-event sender, the fake
/// backend, and the scripted Core (for a second — data-channel — connection).
pub async fn scripted_orchestrator() -> (
    ScriptedCore,
    ScriptedConn,
    mpsc::Sender<BackendEvent>,
    FakeBackend,
) {
    let mut scripted = ScriptedCore::start().await;
    let fake = FakeBackend::default();
    let (backend_tx, backend_rx) = mpsc::channel(16);
    let (client, events) = spawn_client(
        &scripted.path(),
        "spawn-token".into(),
        "clipboard-backend",
        &BACKEND_SCOPES,
        &["clipboard.get_data"],
    );
    tokio::spawn(run(
        client,
        events,
        fake.clone(),
        scripted.path(),
        backend_rx,
        never(),
    ));
    let mut conn = scripted.accept().await;
    conn.handle_hello().await;
    // Resync fires on connect; no live clip here.
    conn.handle_request("clipboard.current", json!({})).await;
    (scripted, conn, backend_tx, fake)
}
