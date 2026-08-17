// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Test harness for the keyboard and mouse engine: TWO real Cores of one
//! account in one process, each running the real engine lib through the real
//! IPC client over a real socket, each on a fake platform backend, each with a
//! second client standing in for an interface.
//!
//! A reduced descendant of the sibling harnesses (`sync/tests/api/support.rs` is
//! the closest, and `client/tests/api/support.rs` is where the enrolled PAIR was
//! proven). The pair is the point: everything this component does that could be
//! got wrong happens between two devices, and a single Core with a scripted peer
//! would prove the shape of the dialect and none of its behaviour.
//!
//! What is real here: both Cores, both sockets, the `input.*` facade's routing,
//! `peers.channel` end to end (including every one of its ten deaths), and
//! `peers.send` for the layout rounds. What is a double: the platform backend
//! ([`onedevice_input::fake::FakeBackend`]), because the OS half is another
//! ticket, and the data plane between the two Cores (the in-memory switchboard,
//! which routes streams by node_id exactly as two iroh endpoints would).

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use onedevice_core::CoreHandle;
use onedevice_input::backend::{BackendEvent, CaptureMode, Monitor};
use onedevice_input::fake::FakeBackend;
pub use onedevice_input::plane::spot_key;
use onedevice_input::{Outcome, SERVED_METHODS, Store};
use onedevice_ipc_client::{Client, ClientConfig, Event, RequestError, TokenSource};
use onedevice_test_support::memory_transport::MemorySwitchboard;
use onedevice_test_support::{
    DeviceKey, FakeOidc, TEST_CLIENT_ID, TEST_EMAIL, TEST_SUB, TestConn, enroll_key,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// Generous on purpose, the menu and sync suites' reasoning: nextest runs every
/// test in its own process, so a whole suite of Cores can be in flight at once on
/// a small runner. It must also exceed the Core's 10 s facade budget, since a
/// client that gave up first would read a slow-but-progressing forward as a
/// failure.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for a state the fleet converges on by itself: a directory snapshot, a
/// warm channel, a layout round. Polled, never awaited blindly.
pub const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(15);
/// Observation window to assert that nothing arrives.
pub const SILENCE_WINDOW: Duration = Duration::from_millis(300);

pub const CORE_DEVICE_NAME: &str = "PC-Core";

/// Copied from `src/main.rs` rather than imported, the sync suite's reasoning:
/// the copy pins the profile the shipping binary is MEANT to ask for,
/// independently of what its own constants say.
pub const ROLE: &str = "input-backend";
pub const SCOPES: [&str; 5] = [
    "input.serve",
    "peers.channel",
    "peers.message",
    "devices.read",
    "session.read",
];
/// Never `input` here: that topic needs `input.read`, which the engine does not
/// hold (it PUBLISHES the topic through `input.emit`), and a refused topic fails
/// the whole subscription.
pub const TOPICS: [&str; 2] = ["devices", "session"];

// ---------------------------------------------------------------------------
// Per-platform IPC paths (the conventions of every sibling suite).
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
        r"\\.\pipe\1device-input-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

// ---------------------------------------------------------------------------
// The real server, so two Cores can be of one account.
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

    /// Enrolls `key` on the test account as a real login's `auth.enroll` does,
    /// and returns the id the account will know that device by.
    async fn enroll(&self, key: &DeviceKey) -> String {
        let mut conn = TestConn::connect(&self.url).await;
        enroll_key(
            &mut conn,
            &self.oidc,
            key,
            TEST_SUB,
            CORE_DEVICE_NAME,
            std::env::consts::OS,
        )
        .await
    }

    fn core_cfg(&self) -> onedevice_core::ServerConfig {
        onedevice_core::ServerConfig {
            url: self.url.clone(),
            oidc_issuer: self.oidc.issuer(),
            oidc_client_id: TEST_CLIENT_ID.into(),
            oidc_client_secret: None,
        }
    }
}

// ---------------------------------------------------------------------------
// A real Core in a temporary directory.
// ---------------------------------------------------------------------------

pub struct TestCore {
    handle: Option<CoreHandle>,
    dir: tempfile::TempDir,
    ipc_path: PathBuf,
    device_id: String,
    node_id: String,
}

impl TestCore {
    /// Two Cores enrolled on the SAME account, sharing one memory switchboard:
    /// they open data-plane streams to each other like two iroh endpoints, each
    /// registered under its real node_id with a synthetic relay to be dialable
    /// by. Same account means the SAME account key (C7): one recovery code,
    /// shared, which each Core attests its own node_id under, and without which
    /// they refuse each other fail-closed.
    ///
    /// Lifted from `client/tests/api/support.rs`, where it was proven for the
    /// live channel's own suite.
    pub async fn start_pair(server: &TestServer) -> (TestCore, TestCore) {
        let switchboard = MemorySwitchboard::new();
        let code = onedevice_core::account_key::generate_recovery_code();
        let a = Self::start_enrolled_on(server, &switchboard, &code).await;
        let b = Self::start_enrolled_on(server, &switchboard, &code).await;
        (a, b)
    }

    /// Two Cores of one account with NO SERVER at all, on one fake local
    /// network, each holding the other's signed record: the shape in which the
    /// account can strike a device off by itself, which is what
    /// `DEVICE_REVOKED` and `ACCOUNT_LEFT` need. Lifted from the live channel's
    /// own suite in the Core, where it was proven.
    pub async fn start_serverless_pair() -> (TestCore, TestCore) {
        let switchboard = MemorySwitchboard::new();
        let code = onedevice_core::account_key::generate_recovery_code();
        let a_key = DeviceKey::generate();
        let b_key = DeviceKey::generate();
        let a_record = peer_record(&a_key, &code);
        let b_record = peer_record(&b_key, &code);
        let a = Self::start_in_account(&code, &switchboard, a_key, &[b_record]).await;
        let b = Self::start_in_account(&code, &switchboard, b_key, &[a_record]).await;
        (a, b)
    }

    async fn start_in_account(
        code: &str,
        switchboard: &MemorySwitchboard,
        key: DeviceKey,
        records: &[Value],
    ) -> TestCore {
        let dir = tempfile::tempdir().expect("tempdir");
        let node_id = key.node_id();
        std::fs::write(dir.path().join("device.key"), key.seed_hex()).expect("seed device.key");
        let ak = onedevice_core::account_key::account_key_from_code(code).expect("valid test code");
        let root = onedevice_core::account_key::root_for(&ak, &node_id);
        onedevice_core::account_key::save(dir.path(), &root).expect("seed account-key.json");
        // The account key itself, as `account.join` stows it: a device that holds
        // it can vouch for another and strike one off, which is the whole point
        // of a serverless pair here.
        onedevice_core::account_key::remember(
            &onedevice_core::FileSecretStore::new(dir.path()),
            &ak,
        )
        .expect("stow the account key");
        // The directory as `account.join` leaves it: this device's own signed
        // record plus whatever it already knows.
        let mut store = vec![peer_record(&key, code)];
        store.extend_from_slice(records);
        let devices: serde_json::Map<String, Value> = store
            .iter()
            .map(|r| {
                (
                    r["device_id"].as_str().expect("device_id").to_string(),
                    r.clone(),
                )
            })
            .collect();
        let saved_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        std::fs::write(
            dir.path().join("directory.json"),
            json!({ "saved_at": saved_at, "devices": devices }).to_string(),
        )
        .expect("seed directory.json");

        let transport: Arc<dyn onedevice_core::PeerTransport> =
            switchboard.endpoint(node_id.clone(), None);
        // Both on the fake LAN, which is the only route a serverless pair has.
        switchboard.join_lan(&node_id);

        let ipc_path = ipc_path_for(dir.path());
        let config = onedevice_core::Config {
            ipc_path: ipc_path.clone(),
            config_dir: dir.path().to_path_buf(),
            server: None,
            config_problem: None,
            reload_server: Arc::new(|| Ok::<_, String>(None)),
            device_name: CORE_DEVICE_NAME.into(),
            secret_store: Arc::new(onedevice_core::FileSecretStore::new(dir.path())),
            connector: Arc::new(onedevice_core::PlainConnector),
            transport,
            receive_dir: dir.path().join("received"),
            reconnect_base_delay: Duration::from_millis(50),
        };
        let handle = onedevice_core::spawn(config).await.expect("Core startup");
        TestCore {
            handle: Some(handle),
            dir,
            // With no server, a device's label IS its node_id.
            device_id: node_id.clone(),
            node_id,
            ipc_path,
        }
    }

    async fn start_enrolled_on(
        server: &TestServer,
        switchboard: &MemorySwitchboard,
        code: &str,
    ) -> TestCore {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = DeviceKey::generate();
        let device_id = server.enroll(&key).await;
        let node_id = key.node_id();
        std::fs::write(dir.path().join("device.key"), key.seed_hex()).expect("seed device.key");
        let session = json!({
            "server_url": server.url,
            "device_id": device_id,
            "account": { "email": TEST_EMAIL },
        });
        std::fs::write(dir.path().join("session.json"), session.to_string())
            .expect("seed session.json");
        // The account's trust root, exactly what `account.join` would write.
        // Without it the data plane is fail-closed: no peer is authorized.
        let ak = onedevice_core::account_key::account_key_from_code(code).expect("valid test code");
        let root = onedevice_core::account_key::root_for(&ak, &node_id);
        onedevice_core::account_key::save(dir.path(), &root).expect("seed account-key.json");
        // A relay to be reached by: the memory transport refuses to connect to a
        // device that published none, exactly as iroh would.
        let transport: Arc<dyn onedevice_core::PeerTransport> =
            switchboard.endpoint(node_id.clone(), Some(format!("iroh+memory://{node_id}")));

        let ipc_path = ipc_path_for(dir.path());
        let config = onedevice_core::Config {
            ipc_path: ipc_path.clone(),
            config_dir: dir.path().to_path_buf(),
            server: Some(server.core_cfg()),
            config_problem: None,
            reload_server: {
                let cfg = server.core_cfg();
                Arc::new(move || Ok::<_, String>(Some(cfg.clone())))
            },
            device_name: CORE_DEVICE_NAME.into(),
            secret_store: Arc::new(onedevice_core::FileSecretStore::new(dir.path())),
            connector: Arc::new(onedevice_core::PlainConnector),
            transport,
            receive_dir: dir.path().join("received"),
            reconnect_base_delay: Duration::from_millis(50),
        };
        let handle = onedevice_core::spawn(config).await.expect("Core startup");
        TestCore {
            handle: Some(handle),
            dir,
            ipc_path,
            device_id,
            node_id,
        }
    }

    pub fn ipc_path(&self) -> PathBuf {
        self.ipc_path.clone()
    }

    /// The id the account knows this Core by: what a `device_id` field carries.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The label the whole account shares, and the one the engine keys
    /// everything by.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Spawn token minted by the Core, the supervisor's bootstrap path.
    pub fn mint(&self, role: &str, scopes: &[&str]) -> String {
        self.handle
            .as_ref()
            .expect("Core stopped")
            .mint_spawn_token(role, scopes)
    }

    /// Where this device's engine keeps its own state, inside the Core's temp
    /// directory so it dies with the test.
    pub fn engine_state_dir(&self) -> PathBuf {
        self.dir.path().join("input-state")
    }

    /// Stops the Core: the socket closes and every channel it carried dies. What
    /// a `SHUTDOWN` and a `PEER_GONE` are staged with.
    pub fn stop(&mut self) {
        self.handle = None;
    }
}

// ---------------------------------------------------------------------------
// Clients.
// ---------------------------------------------------------------------------

pub fn component_config(
    core: &TestCore,
    name: &str,
    role: &str,
    scopes: &[&str],
    topics: &[&str],
    served: &[&str],
) -> ClientConfig {
    ClientConfig {
        ipc_path: core.ipc_path(),
        token: TokenSource::Spawn(core.mint(role, scopes)),
        name: name.into(),
        version: "0.0-test".into(),
        role: role.into(),
        scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
        topics: topics.iter().map(|t| (*t).to_string()).collect(),
        optional_topics: Vec::new(),
        served_methods: served.iter().map(|m| (*m).to_string()).collect(),
        reconnect_base_delay: Duration::from_millis(25),
        // Wide enough for `peers.channel`, whose documented worst case is the
        // Core's whole handshake budget: a tighter one here would turn an honest
        // refusal into a bare timeout, which is the very lie this suite exists
        // to catch.
        request_timeout: RESPONSE_TIMEOUT,
    }
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

/// An interface, holding both facade scopes and subscribed to the `input`
/// topic, with a notification buffer that PUSHES BACK what it skips.
///
/// That last part is not a detail. The waiters in `onedevice-test-support` and in
/// the Core's own harness discard the notifications they step over, so a test
/// that waits for one event and later asserts on another that arrived in between
/// times out on something already thrown away. This one keeps them.
pub struct Ui {
    pub client: Client,
    events: mpsc::Receiver<Event>,
    seen: VecDeque<(String, Value)>,
}

impl Ui {
    pub async fn start(core: &TestCore) -> Ui {
        let (client, mut events) = onedevice_ipc_client::spawn(component_config(
            core,
            "ui",
            "custom",
            &["input.read", "input.manage", "devices.read", "session.read"],
            &["input"],
            &[],
        ));
        expect_connected(&mut events).await;
        Ui {
            client,
            events,
            seen: VecDeque::new(),
        }
    }

    /// A GUI-shaped client for the gestures this suite needs from OUTSIDE the
    /// input facade: logging out, striking a device off the account.
    pub async fn manager(core: &TestCore) -> Ui {
        let (client, mut events) = onedevice_ipc_client::spawn(component_config(
            core,
            "manager",
            "gui",
            &[
                "session.manage",
                "session.read",
                "devices.read",
                "devices.manage",
            ],
            &[],
            &[],
        ));
        expect_connected(&mut events).await;
        Ui {
            client,
            events,
            seen: VecDeque::new(),
        }
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RequestError> {
        self.client.request(method, params).await
    }

    /// The application code of a refused gesture. Panics on a success, because a
    /// test that asked for a refusal and got a result has learned something worth
    /// stopping for.
    pub async fn refusal(&self, method: &str, params: Value) -> String {
        match self.client.request(method, params).await {
            Err(RequestError::Rpc(e)) => e.data_code.unwrap_or_else(|| format!("code {}", e.code)),
            other => panic!("expected a refusal from {method}, got {other:?}"),
        }
    }

    /// Waits for one notification of `method`, keeping everything it steps over.
    pub async fn notification(&mut self, method: &str) -> Value {
        if let Some(pos) = self.seen.iter().position(|(m, _)| m == method) {
            return self.seen.remove(pos).expect("present").1;
        }
        timeout(CONVERGENCE_TIMEOUT, async {
            loop {
                match self.events.recv().await.expect("event channel closed") {
                    Event::Notification { method: m, params } if m == method => return params,
                    Event::Notification { method: m, params } => self.seen.push_back((m, params)),
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timeout waiting for {method}"))
    }

    /// Drains whatever has arrived so far into the buffer, without waiting.
    pub fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            if let Event::Notification { method, params } = event {
                self.seen.push_back((method, params));
            }
        }
    }

    pub fn forget(&mut self) {
        self.drain();
        self.seen.clear();
    }
}

// ---------------------------------------------------------------------------
// One device: a Core, the real engine on a fake backend, and an interface.
// ---------------------------------------------------------------------------

pub struct Device {
    pub core: TestCore,
    pub backend: FakeBackend,
    pub ui: Ui,
    engine: Option<Running>,
}

struct Running {
    task: tokio::task::JoinHandle<Outcome>,
    stop: oneshot::Sender<()>,
}

impl Device {
    /// Starts the real engine on this Core, exactly as `main.rs` wires it: the
    /// documented role and scope profile, a spawn token, and a stop signal
    /// standing in for the supervisor closing standard input.
    pub async fn start(core: TestCore) -> Device {
        let (backend, backend_events) = FakeBackend::new();
        let ui = Ui::start(&core).await;
        let store = Store::open(core.engine_state_dir()).expect("engine store");
        let (client, events) = onedevice_ipc_client::spawn(component_config(
            &core,
            "1device-input",
            ROLE,
            &SCOPES,
            &TOPICS,
            &SERVED_METHODS,
        ));
        let (stop, stop_rx) = oneshot::channel();
        let stdin_closed = async move {
            let _ = stop_rx.await;
        };
        let ipc_path = core.ipc_path();
        let task = tokio::spawn(onedevice_input::run(
            client,
            events,
            backend.clone(),
            backend_events,
            store,
            ipc_path,
            stdin_closed,
        ));
        Device {
            core,
            backend,
            ui,
            engine: Some(Running { task, stop }),
        }
    }

    /// This device's node_id: what the engine keys grants, pins and plane
    /// entries by.
    pub fn node_id(&self) -> &str {
        self.core.node_id()
    }

    pub fn device_id(&self) -> &str {
        self.core.device_id()
    }

    /// The authoritative snapshot, once the engine is actually serving.
    ///
    /// The facade answers `COMPONENT_ABSENT` while the engine's own hello is in
    /// flight, and a loaded runner can outlast one request: both are "not yet",
    /// not "broken". Polls rather than sleeping a fixed amount.
    pub async fn status(&self) -> Value {
        timeout(CONVERGENCE_TIMEOUT, async {
            loop {
                match self.ui.request("input.status", json!({})).await {
                    Ok(status) => break status,
                    Err(RequestError::Rpc(e))
                        if matches!(
                            e.data_code.as_deref(),
                            Some("COMPONENT_ABSENT" | "INPUT_NOT_READY")
                        ) =>
                    {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(RequestError::Timeout | RequestError::NotConnected) => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(e) => panic!("unexpected error waiting for the engine: {e}"),
                }
            }
        })
        .await
        .expect("timeout waiting for the engine to serve its snapshot")
    }

    /// Polls the snapshot until `done` accepts it, and shows the last one it saw
    /// when it never does. Every convergence in this suite goes through here:
    /// nothing sleeps a fixed amount and hopes.
    pub async fn until(&self, what: &str, mut done: impl FnMut(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
        loop {
            let last = self.status().await;
            if done(&last) {
                return last;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timeout waiting for {what}; last snapshot was {last:#}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Polls until this device's row for `device_id` satisfies `done`. The
    /// per-peer half of [`Device::until`], which is where almost every assertion
    /// about a pair lives.
    pub async fn until_peer(
        &self,
        device_id: &str,
        what: &str,
        mut done: impl FnMut(&Value) -> bool,
    ) -> Value {
        let want = device_id.to_string();
        let status = self
            .until(what, |status| {
                peer_row(status, &want).is_some_and(&mut done)
            })
            .await;
        peer_row(&status, &want).cloned().expect("the row is there")
    }

    /// Grants `peer` the right to drive THIS machine. The authority, stored here
    /// and never replicated.
    pub async fn allow(&self, peer: &Device, allowed: bool) {
        self.ui
            .request(
                "input.allow",
                json!({ "device_id": peer.device_id(), "allowed": allowed }),
            )
            .await
            .expect("input.allow");
    }

    /// Says this machine is willing to drive `peer`, which is what gates a warm
    /// channel. It grants nothing on the far side.
    pub async fn drive(&self, peer: &Device, allowed: bool) {
        self.ui
            .request(
                "input.drive",
                json!({ "device_id": peer.device_id(), "allowed": allowed }),
            )
            .await
            .expect("input.drive");
    }

    /// Tells the backend what screens this machine has, and waits for the engine
    /// to have published them.
    pub async fn set_monitors(&self, monitors: Vec<Monitor>) {
        self.backend.set_monitors(monitors.clone());
        self.backend.emit(BackendEvent::MonitorsChanged).await;
        let ids: Vec<String> = monitors.into_iter().map(|m| m.id).collect();
        self.until("the engine to publish its own screens", |status| {
            let here = &status["here"]["monitors"];
            ids.iter().all(|id| {
                here.as_array()
                    .is_some_and(|list| list.iter().any(|m| m["id"] == json!(id)))
            })
        })
        .await;
    }

    /// Sends the graceful stop signal and returns the loop's outcome.
    pub async fn stop_engine(&mut self) -> Outcome {
        let running = self.engine.take().expect("the engine was already stopped");
        let _ = running.stop.send(());
        timeout(RESPONSE_TIMEOUT, running.task)
            .await
            .expect("timeout waiting for the engine to stop")
            .expect("engine task")
    }

    /// Kills the engine WITHOUT its stop signal: what a crash looks like from
    /// the outside, and what the stuck-key crash guard is proven against.
    pub fn kill_engine(&mut self) {
        if let Some(running) = self.engine.take() {
            running.task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// The fleet: two devices of one account, each running the real engine.
// ---------------------------------------------------------------------------

pub struct Fleet {
    /// `None` for a serverless fleet, which is the shape a revocation and a
    /// departure from the account need (no server has to be asked).
    pub server: Option<TestServer>,
    pub a: Device,
    pub b: Device,
}

impl Fleet {
    /// Two devices of one account, both engines running, both having seen each
    /// other in the directory. The starting point of almost every test here.
    pub async fn start() -> Fleet {
        let server = TestServer::start().await;
        let (core_a, core_b) = TestCore::start_pair(&server).await;
        let a = Device::start(core_a).await;
        let b = Device::start(core_b).await;
        let fleet = Fleet {
            server: Some(server),
            a,
            b,
        };
        fleet.converge().await;
        fleet
    }

    /// The same fleet with no server at all, on one fake local network. What a
    /// test needs when the account itself has to strike a device off, or when a
    /// device has to leave the account: both are answered by the account key this
    /// pair holds, with nobody to ask.
    pub async fn start_serverless() -> Fleet {
        let (core_a, core_b) = TestCore::start_serverless_pair().await;
        let a = Device::start(core_a).await;
        let b = Device::start(core_b).await;
        let fleet = Fleet { server: None, a, b };
        fleet.converge().await;
        fleet
    }

    /// Gives each machine its screens and waits until BOTH planes hold BOTH sets.
    ///
    /// Waiting for both ends is not belt and braces, it is the whole point, and
    /// getting it wrong cost this suite half a morning: a machine that publishes
    /// its own screens has not made its sibling know them, and a `input.place` or
    /// an `input.guards` naming a screen the far plane has not learned yet is
    /// refused `INPUT_UNKNOWN_MONITOR` for a perfectly good reason. Every test
    /// that names a monitor across the pair starts here.
    pub async fn screens(&self, a: Vec<Monitor>, b: Vec<Monitor>) {
        self.a.set_monitors(a.clone()).await;
        self.b.set_monitors(b.clone()).await;
        let want: Vec<String> = a
            .iter()
            .map(|m| spot_key(self.a.node_id(), &m.id))
            .chain(b.iter().map(|m| spot_key(self.b.node_id(), &m.id)))
            .collect();
        for (device, whose) in [(&self.a, "A"), (&self.b, "B")] {
            device
                .until(
                    &format!("{whose} to hold every screen of the pair"),
                    |status| {
                        let spots = status["plane"]["spots"].as_array();
                        spots.is_some_and(|spots| {
                            want.iter()
                                .all(|key| spots.iter().any(|s| s["monitor"] == json!(key)))
                        })
                    },
                )
                .await;
        }
    }

    /// [`Fleet::screens`], then A's screen at the origin with B's to its right,
    /// arranged by a human on A.
    ///
    /// Placing them explicitly is not decoration. A plane nobody has dragged
    /// derives its arrangement by appending each undragged machine to the right of
    /// the bounding box in ascending node_id order, so which machine ends up on
    /// the left depends on which node_id sorts first, which is a fresh keypair per
    /// test run. A test that named a side would then pass or fail by coin toss.
    /// Placing also gives the spots a plane a screen can be ABSENT from, which is
    /// what "a screen that is away keeps its place" needs to mean anything.
    pub async fn arranged(&self, a: Vec<Monitor>, b: Vec<Monitor>) {
        self.screens(a.clone(), b.clone()).await;
        let mut spots = Vec::new();
        let mut x = 0;
        for m in &a {
            spots.push(json!({ "monitor": spot_key(self.a.node_id(), &m.id), "x": x, "y": 0 }));
            x += m.w;
        }
        for m in &b {
            spots.push(json!({ "monitor": spot_key(self.b.node_id(), &m.id), "x": x, "y": 0 }));
            x += m.w;
        }
        self.a
            .ui
            .request("input.place", json!({ "spots": spots }))
            .await
            .expect("input.place");
        let by = self.a.device_id().to_string();
        for (device, whose) in [(&self.a, "A"), (&self.b, "B")] {
            device
                .until(&format!("{whose} to hold the arrangement"), |status| {
                    status["plane"]["by"] == json!(by)
                })
                .await;
        }
    }

    /// A live session from A to B with a modifier really held down on B: the
    /// state every teardown test starts from.
    ///
    /// Built out of the public gestures and the fake backends only, so nothing
    /// here reaches inside the engine: B is told it may be driven, A is told it
    /// may drive, both have a screen, A takes the keyboard, and then A's fake
    /// reports a Shift press which B's fake really presses. When this returns,
    /// `b.backend.keys_down()` is non-empty, and that is what a teardown has to
    /// clear.
    pub async fn driving(&self) {
        use onedevice_input::backend::KeyEvent;
        use onedevice_input::keys::{mod_usage, mods};

        // B can produce the modifier A is about to send, and its own Shift key is
        // platform code 16 as far as this test is concerned.
        let shift = mod_usage(mods::SHIFT).expect("shift has a usage");
        self.b.backend.teach_usage(shift, 16);
        self.screens(
            vec![screen("A1", 0, 0, 1920, 1080)],
            vec![screen("B1", 0, 0, 1920, 1080)],
        )
        .await;
        // The authority is B's, and A's willingness is A's.
        self.b.allow(&self.a, true).await;
        self.a.drive(&self.b, true).await;

        // Take the keyboard there explicitly rather than simulating a crossing:
        // the crossing has its own tests, and a teardown test should not depend
        // on one.
        let taken = self
            .a
            .ui
            .request("input.take", json!({ "device_id": self.b.device_id() }))
            .await;
        assert!(taken.is_ok(), "input.take: {taken:?}");
        self.a
            .until_peer(self.b.device_id(), "A to be driving B", |row| {
                row["state"] == json!("driving")
            })
            .await;
        self.b
            .until_peer(self.a.device_id(), "B to be driven by A", |row| {
                row["state"] == json!("driven")
            })
            .await;
        // And wait for A to have PROCESSED the acceptance, which is a different
        // moment from B having sent it. Until then A is only watching, so its
        // keystrokes act locally and are not forwarded, and a modifier emitted in
        // that window is correctly dropped and never re-sent. The moment is
        // unambiguous from the outside: accepting is what makes A start swallowing
        // its own keyboard.
        let source = self.a.backend.clone();
        wait_until("A to start swallowing its own keyboard", || {
            source.calls().capture.last() == Some(&CaptureMode::Swallow)
        })
        .await;

        // A modifier goes down and stays down.
        self.a
            .backend
            .emit(BackendEvent::Key(KeyEvent {
                usage: shift,
                key: None,
                sym: None,
                mods: mods::SHIFT,
                down: true,
                lock: false,
            }))
            .await;
        let held = self.b.backend.clone();
        wait_until("B to be holding the modifier", || {
            !held.keys_down().is_empty()
        })
        .await;
    }

    /// Waits until each device's snapshot names the other as reachable: the
    /// directory has to reach both Cores through the server before anything
    /// between them can be opened.
    pub async fn converge(&self) {
        let b_id = self.b.device_id().to_string();
        let a_id = self.a.device_id().to_string();
        self.a
            .until("A to see B", |status| names_reachable(status, &b_id))
            .await;
        self.b
            .until("B to see A", |status| names_reachable(status, &a_id))
            .await;
    }
}

/// Does this snapshot name `device_id` at all?
fn names_reachable(status: &Value, device_id: &str) -> bool {
    peer_row(status, device_id).is_some()
}

/// One device's row of a snapshot.
pub fn peer_row<'a>(status: &'a Value, device_id: &str) -> Option<&'a Value> {
    status["devices"]
        .as_array()?
        .iter()
        .find(|d| d["device_id"] == json!(device_id))
}

/// Polls a plain condition until it holds, and says what it was waiting for when
/// it never does. For the assertions that read a fake backend's recorded calls
/// rather than a snapshot: nothing in this suite sleeps a fixed amount and hopes.
pub async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A device of the account's signed record, as the account key attests it and as
/// the device itself signed its own description: what a serverless Core finds in
/// its directory on disk. Every piece of this is public Core API.
fn peer_record(key: &DeviceKey, code: &str) -> Value {
    let ak = onedevice_core::account_key::account_key_from_code(code).expect("valid test code");
    let node_id = key.node_id();
    let reach = onedevice_core::Reach::default();
    json!({
        // With no server, a device's label IS its node_id.
        "device_id": node_id,
        "name": CORE_DEVICE_NAME,
        "platform": std::env::consts::OS,
        "node_id": node_id,
        "seq": 1,
        "self_sig": key.sign(&onedevice_core::directory::record_message(
            &node_id,
            CORE_DEVICE_NAME,
            std::env::consts::OS,
            1,
            &reach,
        )),
        "addrs": reach.addrs,
        "relay_hint": reach.relay_hint,
        "relay_url": null,
        "attestation": onedevice_core::account_key::attest(&ak, &node_id),
        "online": false,
        "status": null,
        "last_seen": null,
    })
}

/// One monitor, for a fleet's geometry.
pub fn screen(id: &str, x: i32, y: i32, w: i32, h: i32) -> Monitor {
    Monitor {
        id: id.into(),
        name: format!("Screen {id}"),
        w,
        h,
        x,
        y,
        scale: 1000,
        primary: x == 0 && y == 0,
    }
}
