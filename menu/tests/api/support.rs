// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Test harness for the contextual-menu manager.
//!
//! The whole chain is real and in-process: manager → `onedevice-ipc-client` →
//! a real Core → a real `1device-server` + `FakeOidc`. No scripted Core
//! here, and that is the point — every rule the manager applies is about *device
//! records*, so the records have to be genuine: enrolled through the real
//! enrollment, brought online by a real `auth.authenticate`, and given a real
//! account attestation computed with the Core's own account key. Hand-written
//! JSON would let a wrong filter pass.
//!
//! What the harness owns:
//! - [`TestServer`] — server + fake IdP, and the peers ("the account's other
//!   PCs") it can bring online, with or without an attestation.
//! - [`TestCore`] — a Core in a temp directory, pointed at that server.
//! - [`login`] — the real OIDC flow plus `account.setup`, which returns the
//!   recovery code the harness needs to attest a peer the Core will accept.
//! - [`Manager`] — the component under test: a spawn token with the role and
//!   scopes the real binary asks for, a [`FakeSurface`] instead of a desktop, and
//!   a channel on a temp path so a click can be played end to end.
//!
//! Conventions frozen here, so a later brick does not quietly change them:
//! - the manager's identity is exactly what `main.rs` sends ([`ROLE`],
//!   [`SCOPES`], [`TOPICS`]) — the harness must never grant itself more, or the
//!   suite would stop proving the real binary can connect;
//! - a render is observed by POLLING the fake surface, never by a fixed sleep:
//!   the orchestrator debounces, so the interesting assertion is always "the list
//!   settles on this", and a sleep would either be flaky or slow;
//! - the assertions are on the LAST render. Intermediate ones are an
//!   implementation detail of the debounce and are deliberately not pinned.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use onedevice_core::CoreHandle;
use onedevice_ipc_client::{ClientConfig, Event, TokenSource};
use onedevice_menu::channel::{self, Request, Response};
use onedevice_menu::{MenuSurface, Outcome, Target};
use onedevice_test_support::{
    Device, FakeOidc, TEST_CLIENT_ID, TEST_SUB, authenticate, browse, enroll_device_at,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// Deliberately generous. This suite stands up a server, a fake IdP, a Core and
/// a real OIDC round trip per test, and nextest runs each test in its own process
/// — on a loaded 3-core macOS or Windows runner that is slow. A passing test never
/// waits, so the only cost of a wide bound is how long a genuine failure takes to
/// report; a tight one buys a red CI instead (the GUI suite's 5 s bound already
/// timed out once on windows-latest).
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long an assertion polls the fake surface for the list it expects. Well
/// above the orchestrator's 250 ms debounce, and above a snapshot retry.
pub const SETTLE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to watch for something that must NOT happen.
pub const SILENCE_WINDOW: Duration = Duration::from_millis(600);
pub const CORE_DEVICE_NAME: &str = "PC-Core";

/// The manager's identity, copied from `menu/src/main.rs`. If the two drift, the
/// suite stops covering the binary that actually ships.
pub const ROLE: &str = "menu-backend";
pub const SCOPES: &[&str] = &["session.read", "devices.read", "files.send"];
pub const TOPICS: &[&str] = &["session", "devices"];

// ---------------------------------------------------------------------------
// Per-platform paths (same conventions as the Core and clipboard suites).
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn ipc_path_for(dir: &Path) -> PathBuf {
    dir.join("core.sock")
}

#[cfg(windows)]
fn ipc_path_for(_dir: &Path) -> PathBuf {
    PathBuf::from(format!(
        r"\\.\pipe\1device-menu-test-core-{}-{}",
        std::process::id(),
        next_seq()
    ))
}

#[cfg(unix)]
fn channel_path_for(dir: &Path) -> PathBuf {
    dir.join("menu.sock")
}

#[cfg(windows)]
fn channel_path_for(_dir: &Path) -> PathBuf {
    PathBuf::from(format!(
        r"\\.\pipe\1device-menu-test-channel-{}-{}",
        std::process::id(),
        next_seq()
    ))
}

#[cfg(windows)]
fn next_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The real server + fake IdP.
// ---------------------------------------------------------------------------

pub struct TestServer {
    pub oidc: FakeOidc,
    _server: onedevice_server::ServerHandle,
    url: String,
}

impl TestServer {
    pub async fn start() -> TestServer {
        let oidc = FakeOidc::start().await;
        let config = onedevice_server::Config {
            bind_addr: "127.0.0.1:0".parse().expect("addr"),
            oidc: onedevice_server::OidcConfig {
                issuer_url: oidc.issuer(),
                client_id: TEST_CLIENT_ID.into(),
                client_secret: None,
                max_fresh_token_age: Duration::from_secs(300),
                jwks_refresh_min_interval: Duration::from_secs(60),
            },
            heartbeat_interval: Duration::from_secs(30),
            heartbeat_max_missed: 2,
            nonce_ttl: Duration::from_secs(60),
            pairing_ttl: Duration::from_secs(120),
            max_requests_per_minute: None,
            relays: Vec::new(),
            relay_max_payload: None,
        };
        let server = onedevice_server::spawn(config)
            .await
            .expect("server startup");
        let url = format!("ws://{}/ws", server.local_addr());
        TestServer {
            oidc,
            _server: server,
            url,
        }
    }

    pub fn core_cfg(&self) -> onedevice_core::ServerConfig {
        onedevice_core::ServerConfig {
            url: self.url.clone(),
            oidc_issuer: self.oidc.issuer(),
            oidc_client_id: TEST_CLIENT_ID.into(),
            oidc_client_secret: None,
        }
    }

    /// Another PC of the account: enrolled and online, but with NO account
    /// attestation — exactly the state a device is in before it joins the vault.
    /// `files.send` refuses it, so the menu must not offer it.
    pub async fn unattested_peer(&self, name: &str, platform: &str) -> Device {
        let mut d = enroll_device_at(&self.url, &self.oidc, TEST_SUB, name, platform).await;
        authenticate(&mut d.conn, &d.key, &d.device_id).await;
        d
    }

    /// Another PC of the account: online AND attested under `recovery_code`'s
    /// account key, with a published relay — everything `files.send` requires to
    /// resolve it.
    pub async fn attested_peer(&self, recovery_code: &str, name: &str, platform: &str) -> Device {
        let mut d = self.unattested_peer(name, platform).await;
        attest(&mut d, recovery_code).await;
        d
    }
}

/// Publishes a valid attestation (and a relay) for `device`, computed with the
/// real account key derived from the recovery code — the same primitive the Core
/// uses, so the Core genuinely accepts it.
pub async fn attest(device: &mut Device, recovery_code: &str) {
    let ak = onedevice_core::account_key::account_key_from_code(recovery_code)
        .expect("account key from the recovery code");
    let attestation = onedevice_core::account_key::attest(&ak, &device.key.node_id());
    device
        .conn
        .request(
            "presence.update",
            json!({ "attestation": attestation, "relay_url": "https://relay.test/" }),
        )
        .await
        .expect("presence.update");
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
    pub async fn start(server: &TestServer) -> TestCore {
        let dir = tempfile::tempdir().expect("tempdir");
        let ipc_path = ipc_path_for(dir.path());
        let server_cfg = server.core_cfg();
        let config = onedevice_core::Config {
            ipc_path: ipc_path.clone(),
            config_dir: dir.path().to_path_buf(),
            server: Some(server_cfg.clone()),
            config_problem: None,
            reload_server: Arc::new(move || Ok::<_, String>(Some(server_cfg.clone()))),
            device_name: CORE_DEVICE_NAME.into(),
            secret_store: Arc::new(onedevice_core::FileSecretStore::new(dir.path())),
            connector: Arc::new(onedevice_core::PlainConnector),
            // The menu suite never runs a transfer: a click is proven by the
            // `files.send` the Core accepts, not by bytes moving (that is T2's
            // suite). An isolated in-memory transport keeps this suite off the
            // cross-reactor contention that needs nextest serialization.
            transport: onedevice_test_support::memory_transport::MemorySwitchboard::new()
                .endpoint("menu-test", None),
            receive_dir: dir.path().join("received"),
            reconnect_base_delay: Duration::from_millis(50),
        };
        let handle = onedevice_core::spawn(config).await.expect("Core startup");
        TestCore {
            handle: Some(handle),
            dir,
            ipc_path,
        }
    }

    pub fn ipc_path(&self) -> PathBuf {
        self.ipc_path.clone()
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Bootstrap path B: a spawn token minted by the Core, exactly as the
    /// supervisor would.
    pub fn mint(&self, role: &str, scopes: &[&str]) -> String {
        self.handle
            .as_ref()
            .expect("Core stopped")
            .mint_spawn_token(role, scopes)
    }

    /// Stops the Core, which is how the suite makes the manager lose its IPC.
    pub fn stop(&mut self) {
        self.handle = None;
    }
}

/// Logs the Core in through the real OIDC flow and creates the account key.
/// Returns the recovery code — the harness needs it to attest peers the Core will
/// accept.
pub async fn login(core: &TestCore) -> String {
    let token = core.mint("gui", &["session.read", "session.manage", "devices.read"]);
    let (client, mut events) = onedevice_ipc_client::spawn(ClientConfig {
        ipc_path: core.ipc_path(),
        token: TokenSource::Spawn(token),
        name: "harness-gui".into(),
        version: "0".into(),
        role: "gui".into(),
        scopes: vec![
            "session.read".into(),
            "session.manage".into(),
            "devices.read".into(),
        ],
        topics: vec!["session".into()],
        optional_topics: Vec::new(),
        served_methods: vec![],
        reconnect_base_delay: Duration::from_millis(50),
        request_timeout: RESPONSE_TIMEOUT,
    });
    expect_connected(&mut events).await;

    let r = client
        .request("session.login", json!({}))
        .await
        .expect("session.login");
    let auth_url = r["auth_url"].as_str().expect("auth_url").to_string();
    let page = browse(&auth_url).await.expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);

    // The session opens then converges: logged_in first, the server connection
    // next. Both are needed before the account key can be published.
    let mut p = wait_notification(&mut events, "session.changed").await;
    while p["server_connected"] != json!(true) {
        p = wait_notification(&mut events, "session.changed").await;
        assert_eq!(p["logged_in"], true, "{p}");
    }

    let r = client
        .request("account.setup", json!({}))
        .await
        .expect("account.setup");
    r["recovery_code"]
        .as_str()
        .expect("recovery_code")
        .to_string()
}

pub async fn expect_connected(events: &mut tokio::sync::mpsc::Receiver<Event>) -> Vec<String> {
    loop {
        match tokio::time::timeout(RESPONSE_TIMEOUT, events.recv()).await {
            Ok(Some(Event::Connected { granted_scopes, .. })) => return granted_scopes,
            Ok(Some(_)) => {}
            Ok(None) => panic!("client ended before connecting"),
            Err(_) => panic!("no Connected event"),
        }
    }
}

pub async fn wait_notification(
    events: &mut tokio::sync::mpsc::Receiver<Event>,
    method: &str,
) -> Value {
    let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(Event::Notification { method: m, params })) if m == method => return params,
            Ok(Some(_)) => {}
            Ok(None) => panic!("client ended while waiting for {method}"),
            Err(_) => panic!("no {method}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The fake OS surface.
// ---------------------------------------------------------------------------

/// Records every list it is asked to render.
#[derive(Clone, Default)]
pub struct Renders(Arc<Mutex<Vec<Vec<Target>>>>);

impl Renders {
    pub fn all(&self) -> Vec<Vec<Target>> {
        self.0.lock().expect("renders").clone()
    }

    pub fn count(&self) -> usize {
        self.0.lock().expect("renders").len()
    }

    /// The list the surface is showing right now.
    pub fn current(&self) -> Vec<Target> {
        self.0
            .lock()
            .expect("renders")
            .last()
            .cloned()
            .unwrap_or_default()
    }

    pub fn current_ids(&self) -> Vec<String> {
        self.current().into_iter().map(|t| t.device_id).collect()
    }

    pub fn current_names(&self) -> Vec<String> {
        self.current().into_iter().map(|t| t.name).collect()
    }
}

pub struct FakeSurface(Renders);

impl MenuSurface for FakeSurface {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn apply(&mut self, targets: &[Target]) -> std::io::Result<()> {
        self.0.lock_push(targets.to_vec());
        Ok(())
    }
}

impl Renders {
    fn lock_push(&self, targets: Vec<Target>) {
        self.0.lock().expect("renders").push(targets);
    }
}

// ---------------------------------------------------------------------------
// The component under test.
// ---------------------------------------------------------------------------

pub struct Manager {
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Outcome>>,
    pub renders: Renders,
    channel_path: PathBuf,
}

impl Manager {
    /// Starts the manager against `core`, with the role and scopes the real
    /// binary asks for, rendering onto a [`FakeSurface`] only.
    pub async fn start(core: &TestCore) -> Manager {
        Manager::start_with(core, |_| Vec::new()).await
    }

    /// Starts the manager against `core` with the surfaces `surfaces` builds from
    /// the channel path it will listen on — which is what a surface needs, since a
    /// generated artifact has to point a courier at THIS manager.
    ///
    /// A [`FakeSurface`] is always appended LAST, so [`Manager::await_targets`]
    /// stays the synchronization point even for real surfaces: the applier drives
    /// them in order, so by the time the fake has recorded a list the others have
    /// already written it to disk.
    pub async fn start_with(
        core: &TestCore,
        surfaces: impl FnOnce(&Path) -> Vec<Box<dyn MenuSurface>>,
    ) -> Manager {
        let renders = Renders::default();
        let channel_path = channel_path_for(core.dir());
        let listener = channel::bind(&channel_path).expect("bind the local channel");
        let mut surfaces = surfaces(&channel_path);

        let (client, events) = onedevice_ipc_client::spawn(ClientConfig {
            ipc_path: core.ipc_path(),
            token: TokenSource::Spawn(core.mint(ROLE, SCOPES)),
            name: "1device-menu".into(),
            version: "0".into(),
            role: ROLE.into(),
            scopes: SCOPES.iter().map(|s| (*s).to_string()).collect(),
            topics: TOPICS.iter().map(|t| (*t).to_string()).collect(),
            optional_topics: Vec::new(),
            served_methods: vec![],
            reconnect_base_delay: Duration::from_millis(50),
            request_timeout: RESPONSE_TIMEOUT,
        });

        let (stop_tx, stop_rx) = oneshot::channel();
        surfaces.push(Box::new(FakeSurface(renders.clone())));
        let task = tokio::spawn(onedevice_menu::run(
            client,
            events,
            async move {
                let _ = stop_rx.await;
            },
            listener,
            surfaces,
        ));

        Manager {
            stop: Some(stop_tx),
            task: Some(task),
            renders,
            channel_path,
        }
    }

    pub fn channel_path(&self) -> &Path {
        &self.channel_path
    }

    /// Waits until the surface shows exactly these device ids. Polls, because the
    /// orchestrator debounces.
    ///
    /// Also waits for at least one render to have happened: an empty expectation
    /// would otherwise be satisfied by a surface that has never been touched,
    /// which is not the same claim at all.
    pub async fn await_targets(&self, expected: &[&str]) {
        let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            let current = self.renders.current_ids();
            if self.renders.count() > 0 && current == expected {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("surface shows {current:?}, expected {expected:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Asserts the surface stays on `expected` for a while: what proves a change
    /// was NOT rendered.
    pub async fn assert_stable(&self, expected: &[&str]) {
        self.await_targets(expected).await;
        let renders = self.renders.count();
        tokio::time::sleep(SILENCE_WINDOW).await;
        assert_eq!(
            self.renders.current_ids(),
            expected,
            "the list moved during the silence window"
        );
        assert_eq!(
            self.renders.count(),
            renders,
            "the surface was rewritten with no change to show"
        );
    }

    /// Plays a click: exactly what the `--send` helper does.
    pub async fn click(&self, device_id: &str, paths: &[PathBuf]) -> Response {
        channel::request(
            &self.channel_path,
            &Request::Send {
                device_id: device_id.to_string(),
                paths: paths.to_vec(),
            },
        )
        .await
        .expect("channel exchange")
    }

    /// The family-B pull.
    pub async fn ask_targets(&self) -> Response {
        channel::request(&self.channel_path, &Request::Targets)
            .await
            .expect("channel exchange")
    }

    /// Signals the supervisor's graceful stop (standard input EOF) and returns
    /// why the manager ended.
    pub async fn stop(mut self) -> Outcome {
        drop(self.stop.take());
        self.wait().await
    }

    /// Waits for the manager to end on its own (a lost connection, an
    /// incompatibility).
    pub async fn wait(&mut self) -> Outcome {
        let task = self.task.take().expect("already awaited");
        tokio::time::timeout(RESPONSE_TIMEOUT, task)
            .await
            .expect("the manager did not stop in time")
            .expect("manager task")
    }
}

/// Watches the Core's `transfers` topic. This is how a test sees what the Core
/// was ACTUALLY asked to send: `transfer.started` carries the target device and
/// the frozen manifest, and it is emitted before the data plane is even dialed
/// (`core/src/dataplane.rs`), so it arrives even though the harness peer is not a
/// real endpoint.
pub struct TransferWatcher {
    _client: onedevice_ipc_client::Client,
    events: tokio::sync::mpsc::Receiver<Event>,
}

impl TransferWatcher {
    pub async fn connect(core: &TestCore) -> TransferWatcher {
        let (client, mut events) = onedevice_ipc_client::spawn(ClientConfig {
            ipc_path: core.ipc_path(),
            token: TokenSource::Spawn(core.mint("custom", &["transfers.read"])),
            name: "harness-transfers".into(),
            version: "0".into(),
            role: "custom".into(),
            scopes: vec!["transfers.read".into()],
            topics: vec!["transfers".into()],
            optional_topics: Vec::new(),
            served_methods: vec![],
            reconnect_base_delay: Duration::from_millis(50),
            request_timeout: RESPONSE_TIMEOUT,
        });
        expect_connected(&mut events).await;
        TransferWatcher {
            _client: client,
            events,
        }
    }

    /// The next `transfer.started` payload.
    pub async fn started(&mut self) -> Value {
        wait_notification(&mut self.events, "transfer.started").await
    }

    /// Whether ANOTHER `transfer.started` shows up within `window`. What proves a
    /// gesture did not become several transfers — an assertion no shape of a reply
    /// could make.
    pub async fn another_within(&mut self, window: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(Event::Notification { method, params }))
                    if method == "transfer.started" =>
                {
                    return Some(params);
                }
                // A failure notification is expected here: the harness peer is not
                // a real data-plane endpoint.
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return None,
            }
        }
    }

    /// The basenames the Core froze into the manifest, in order.
    pub fn manifest_names(started: &Value) -> Vec<String> {
        started["files"]
            .as_array()
            .expect("manifest")
            .iter()
            .map(|f| f["name"].as_str().expect("name").to_string())
            .collect()
    }
}

/// Sends a raw line on the channel and returns the raw reply — the only way to
/// exercise what the manager does with a request the typed client could not
/// build (a foreign version, a relative path, an oversized line).
pub async fn raw_exchange(path: &Path, line: &str) -> String {
    try_raw_exchange(path, line)
        .await
        .expect("the raw exchange failed")
}

/// Same, but tolerating a failure. A request the manager refuses mid-write — an
/// oversized one — legitimately breaks the pipe: the manager answers and hangs up
/// while the courier is still writing, which is fail-fast, not a defect.
pub async fn try_raw_exchange(path: &Path, line: &str) -> Option<String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut stream = channel::connect(path)
        .await
        .expect("connect to the channel");
    let written = stream.write_all(line.as_bytes()).await;
    let _ = stream.flush().await;
    let mut reply = String::new();
    let mut reader = BufReader::new(&mut stream);
    let read = tokio::time::timeout(RESPONSE_TIMEOUT, reader.read_line(&mut reply))
        .await
        .expect("the manager did not reply in time");
    match (written, read) {
        (Ok(()), Ok(n)) if n > 0 => Some(reply),
        // Either the write broke or nothing came back.
        _ if !reply.is_empty() => Some(reply),
        _ => None,
    }
}

/// Runs the REAL courier binary, exactly as a menu entry invokes it: the same
/// executable, the same argument grammar, pointed at this manager's channel.
///
/// `tokio::process`, never `std::process`: these tests run on a single-threaded
/// runtime, and blocking it would stop the in-process manager from ever answering
/// the courier it is waiting for.
pub async fn run_courier(
    channel: &Path,
    device_id: &str,
    paths: &[PathBuf],
) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_1device-menu"));
    command
        .arg("--channel")
        .arg(channel)
        .arg("--send")
        .arg(device_id)
        .arg("--");
    for path in paths {
        command.arg(path);
    }
    tokio::time::timeout(RESPONSE_TIMEOUT, command.output())
        .await
        .expect("the courier did not finish in time")
        .expect("the courier could not be started")
}

/// A file the harness can legitimately ask to send.
pub fn a_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"hello").expect("write test file");
    path
}
