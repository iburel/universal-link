// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Test harness for the sync engine. A REDUCED copy of the sibling harnesses
//! (clipboard/tests/api/support.rs is the closest, 5th copy - to be extracted
//! into test-support if it keeps growing): a real Core in a temp directory,
//! the real engine lib connected through the real IPC client, and a second
//! client standing in for an interface holding the facade scopes.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use onedevice_core::CoreHandle;
use onedevice_ipc_client::{Client, ClientConfig, Event, RequestError, TokenSource};
use onedevice_sync::{Outcome, SERVED_METHODS, Store};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// Generous on purpose (the menu suite's reasoning): nextest runs every
/// test in its own process, so a whole suite of Cores can be in flight at
/// once on a small runner. It must also exceed the Core's 10 s facade
/// budget - a client that gave up first would read a slow-but-progressing
/// forward as a failure.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);
pub const CORE_DEVICE_NAME: &str = "PC-Core";

/// Copied from `src/main.rs` rather than imported: the copy pins the
/// profile the shipping binary is MEANT to ask for, independently of what
/// its own constants say. (The suite drives the lib, so it cannot detect a
/// drift in the binary's copy; what it proves is that this profile, the
/// documented one, enrolls and serves.)
pub const ROLE: &str = "sync-backend";
pub const SCOPES: [&str; 6] = [
    "sync.serve",
    "transactions.publish",
    "peers.message",
    "devices.read",
    "session.read",
    "transfers.read",
];
pub const TOPICS: [&str; 3] = ["devices", "session", "transfers"];

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
        r"\\.\pipe\1device-sync-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

// ---------------------------------------------------------------------------
// The real Core in a temporary directory.
// ---------------------------------------------------------------------------

pub struct TestCore {
    handle: CoreHandle,
    dir: tempfile::TempDir,
    ipc_path: PathBuf,
}

impl TestCore {
    pub async fn start() -> TestCore {
        TestCore::start_with(|_| {}).await
    }

    /// Starts the Core after `seed` prepared its config directory (a device
    /// key, an account root: whatever the scenario needs on disk first).
    pub async fn start_with(seed: impl FnOnce(&Path)) -> TestCore {
        let dir = tempfile::tempdir().expect("tempdir");
        seed(dir.path());
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
            transport: onedevice_test_support::memory_transport::MemorySwitchboard::new()
                .endpoint("sync-test", None),
            receive_dir: dir.path().join("received"),
            reconnect_base_delay: Duration::from_millis(50),
        };
        let handle = onedevice_core::spawn(config).await.expect("Core startup");
        TestCore {
            handle,
            dir,
            ipc_path,
        }
    }

    pub fn ipc_path(&self) -> PathBuf {
        self.ipc_path.clone()
    }

    /// Spawn token minted by the Core for `role`/`scopes`.
    pub fn mint(&self, role: &str, scopes: &[&str]) -> String {
        self.handle.mint_spawn_token(role, scopes)
    }

    /// Where the engine under test keeps its state (inside the Core's temp
    /// dir, so it dies with the test).
    pub fn engine_state_dir(&self) -> PathBuf {
        self.dir.path().join("engine-state")
    }
}

// ---------------------------------------------------------------------------
// Client helpers.
// ---------------------------------------------------------------------------

pub fn spawn_client(
    core: &TestCore,
    role: &str,
    scopes: &[&str],
    topics: &[&str],
    served: &[&str],
) -> (Client, mpsc::Receiver<Event>) {
    onedevice_ipc_client::spawn(ClientConfig {
        ipc_path: core.ipc_path(),
        token: TokenSource::Spawn(core.mint(role, scopes)),
        name: "sync-test".into(),
        version: "0.0-test".into(),
        role: role.into(),
        scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
        topics: topics.iter().map(|t| (*t).to_string()).collect(),
        optional_scopes: Vec::new(),
        optional_topics: Vec::new(),
        served_methods: served.iter().map(|m| (*m).to_string()).collect(),
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

// ---------------------------------------------------------------------------
// The engine under test: the real lib, driven like the binary drives it.
// ---------------------------------------------------------------------------

pub struct Engine {
    task: tokio::task::JoinHandle<Outcome>,
    stop: oneshot::Sender<()>,
}

impl Engine {
    /// Starts the real orchestrator with the shipping role/scope profile,
    /// exactly as `main.rs` wires it - the stop signal stands in for the
    /// supervisor closing standard input.
    pub fn start(core: &TestCore) -> Engine {
        let store = Store::open(core.engine_state_dir()).expect("engine store");
        let (client, events) = spawn_client(core, ROLE, &SCOPES, &TOPICS, &SERVED_METHODS);
        let (stop, stop_rx) = oneshot::channel();
        let stdin_closed = async move {
            let _ = stop_rx.await;
        };
        let task = tokio::spawn(onedevice_sync::run(
            client,
            events,
            store,
            stdin_closed,
            Duration::from_millis(200),
        ));
        Engine { task, stop }
    }

    /// Sends the graceful-stop signal and returns the loop's outcome.
    pub async fn stop(self) -> Outcome {
        let _ = self.stop.send(());
        timeout(RESPONSE_TIMEOUT, self.task)
            .await
            .expect("timeout waiting for the engine to stop")
            .expect("engine task")
    }
}

// ---------------------------------------------------------------------------
// The interface double.
// ---------------------------------------------------------------------------

pub struct Ui {
    pub client: Client,
    pub events: mpsc::Receiver<Event>,
}

/// An interface holding both facade scopes, subscribed to the `sync` topic.
pub async fn ui(core: &TestCore) -> Ui {
    let (client, mut events) = spawn_client(
        core,
        "custom",
        &["sync.read", "sync.manage"],
        &["sync"],
        &[],
    );
    expect_connected(&mut events).await;
    Ui { client, events }
}

/// A serverless Core that joined an account: device key + account root
/// seeded on disk, exactly what `account.setup` would have written. The
/// engine resolves its own node_id from such a Core's directory.
pub async fn core_in_account() -> TestCore {
    TestCore::start_with(|dir| {
        let key = onedevice_test_support::DeviceKey::generate();
        std::fs::write(dir.join("device.key"), key.seed_hex()).expect("seed the device key");
        let code = onedevice_core::account_key::generate_recovery_code();
        let ak = onedevice_core::account_key::account_key_from_code(&code).expect("account key");
        let root = onedevice_core::account_key::root_for(&ak, &key.node_id());
        onedevice_core::account_key::save(dir, &root).expect("save the account root");
    })
    .await
}

/// Calls a gesture until the engine has resolved its directory: the very
/// first calls answer `SYNC_NOT_READY` while the resolution is in flight,
/// and the suite never sleeps a fixed amount.
pub async fn ready(ui: &Ui) {
    timeout(RESPONSE_TIMEOUT, async {
        loop {
            match ui.client.request("sync.status", json!({})).await {
                Ok(_) => {
                    // Status answers even unresolved; probe a gesture that
                    // needs the engine.
                    match ui
                        .client
                        .request("sync.pause", json!({ "set_id": "AAAAAAAAAAAAAAAAAAAAAA" }))
                        .await
                    {
                        Err(RequestError::Rpc(e))
                            if e.data_code.as_deref() == Some("SYNC_UNKNOWN_SET") =>
                        {
                            return;
                        }
                        _ => tokio::time::sleep(Duration::from_millis(25)).await,
                    }
                }
                // The engine's hello may still be in flight, and a loaded
                // runner can outlast one request: both are "not yet", not
                // "broken".
                Err(RequestError::Rpc(e)) if e.data_code.as_deref() == Some("COMPONENT_ABSENT") => {
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
    .expect("timeout waiting for the engine to resolve its directory")
}

/// The application code of a refused gesture.
pub async fn app_code(ui: &Ui, method: &str, params: Value) -> String {
    match ui.client.request(method, params).await {
        Err(RequestError::Rpc(e)) => e.data_code.unwrap_or_else(|| format!("code {}", e.code)),
        other => panic!("expected a refusal from {method}, got {other:?}"),
    }
}

/// Waits for one `sync.*` notification of the given method.
pub async fn wait_notification(ui: &mut Ui, method: &str) -> Value {
    timeout(RESPONSE_TIMEOUT, async {
        loop {
            match ui.events.recv().await.expect("event channel closed") {
                Event::Notification { method: m, params } if m == method => return params,
                _ => continue,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timeout waiting for {method}"))
}

/// Calls `sync.status` until the engine is actually serving: the facade
/// answers `COMPONENT_ABSENT` while the engine's own hello is still in
/// flight, and the suite never sleeps a fixed amount.
pub async fn status_eventually(ui: &Ui) -> Value {
    timeout(RESPONSE_TIMEOUT, async {
        loop {
            match ui.client.request("sync.status", json!({})).await {
                Ok(status) => break status,
                // The engine's hello may still be in flight, and a loaded
                // runner can outlast one request: both are "not yet", not
                // "broken".
                Err(RequestError::Rpc(e)) if e.data_code.as_deref() == Some("COMPONENT_ABSENT") => {
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
    .expect("timeout waiting for the engine to serve the facade")
}
