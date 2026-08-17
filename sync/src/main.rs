// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The `1device-sync` binary: wiring for the official sync engine.
//!
//! Supervised-component contract (see `daemon/src/supervisor.rs`): find the
//! Core at `ONEDEVICE_IPC_PATH`, read the single-use spawn token from the
//! first line of standard input, keep that standard input open (its EOF means
//! "stop"), and exit if the IPC connection drops (the token is single-use, so
//! a reconnection would fail - exiting lets the supervisor restart us fresh).
//!
//! All the testable logic lives in the lib; this is wiring.

use std::path::PathBuf;
use std::time::Duration;

use onedevice_ipc_client::{ClientConfig, TokenSource};
use onedevice_sync::orchestrator::SAFETY_TICK;
use onedevice_sync::{Outcome, SERVED_METHODS, Store, run};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

/// Set by the supervisor: the Core's listening endpoint.
const IPC_PATH_ENV: &str = "ONEDEVICE_IPC_PATH";
/// Barely matters: a spawn token is single-use, so we exit on the first loss
/// rather than let the client retry.
const RECONNECT_BASE_DELAY: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The role and rights the engine asks for: exactly the documented profile
/// (doc/core-api.md, "Scopes"). `sync.serve` answers the facade,
/// `transactions.publish` and `peers.message` are the two primitives the
/// protocol rides, and the three read scopes are the directory, the session
/// and the fill outcomes.
const ROLE: &str = "sync-backend";
const SCOPES: [&str; 6] = [
    "sync.serve",
    "transactions.publish",
    "peers.message",
    "devices.read",
    "session.read",
    "transfers.read",
];
/// Never `sync` here: that topic requires `sync.read`, which the engine does
/// not hold (it PUBLISHES the topic through `sync.emit`), and a refused topic
/// fails the whole subscription.
const TOPICS: [&str; 3] = ["devices", "session", "transfers"];

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(engine());
    // NOT a plain drop: `Runtime::drop` waits for every blocking task, and the
    // engine leaves one behind for ever - the read on standard input that is
    // its stop signal, which only ends when the supervisor closes it (the menu
    // manager's discovery, menu/src/main.rs).
    runtime.shutdown_background();
    std::process::exit(code);
}

async fn engine() -> i32 {
    let Ok(ipc_path) = std::env::var(IPC_PATH_ENV) else {
        eprintln!("[1device-sync] {IPC_PATH_ENV} is not set: the engine is launched by the Core");
        return 1;
    };
    let Some(data_dir) = onedevice_paths::data_dir() else {
        eprintln!("[1device-sync] cannot resolve the data directory from the environment");
        return 1;
    };
    let store = match Store::open(data_dir.join("sync")) {
        Ok(store) => store,
        // A corrupt identity is deliberately NOT self-healing (a silently
        // fresh key would unpin this device everywhere): report and stay
        // down, visibly, until a human decides.
        Err(e) => {
            eprintln!("[1device-sync] cannot open the engine state: {e}");
            return 1;
        }
    };

    // Contract: the spawn token is the FIRST LINE of standard input - never
    // argv (world-readable) nor the environment (inherited by descendants).
    // Standard input then stays open; its EOF is the graceful-stop signal.
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut token = String::new();
    match stdin.read_line(&mut token).await {
        Ok(_) if !token.trim().is_empty() => {}
        _ => {
            eprintln!("[1device-sync] no spawn token on standard input");
            return 1;
        }
    }
    let token = token.trim().to_string();

    let (client, events) = onedevice_ipc_client::spawn(ClientConfig {
        ipc_path: PathBuf::from(ipc_path),
        token: TokenSource::Spawn(token),
        name: "1device-sync".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: ROLE.into(),
        scopes: SCOPES.iter().map(|s| (*s).to_string()).collect(),
        topics: TOPICS.iter().map(|t| (*t).to_string()).collect(),
        optional_scopes: Vec::new(),
        optional_topics: Vec::new(),
        served_methods: SERVED_METHODS.iter().map(|m| (*m).to_string()).collect(),
        reconnect_base_delay: RECONNECT_BASE_DELAY,
        request_timeout: REQUEST_TIMEOUT,
    });

    // The rest of standard input: reading it to EOF is the stop signal.
    let stdin_closed = async move {
        let mut sink = Vec::new();
        let _ = stdin.read_to_end(&mut sink).await;
    };

    match run(client, events, store, stdin_closed, SAFETY_TICK).await {
        Outcome::StdinClosed => 0,
        // IPC lost / incompatible / client ended: exit non-zero so the
        // supervisor restarts us with a fresh, single-use spawn token.
        Outcome::ConnectionLost | Outcome::Incompatible | Outcome::ClientEnded => 1,
    }
}
