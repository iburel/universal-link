// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The `universallink-tray` binary: wiring only (standard input, environment,
//! IPC client). The logic — and its exit conditions — live in the lib.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use universallink_ipc_client::{ClientConfig, TokenSource};
use universallink_tray::{Outcome, run};

/// Set by the supervisor: the Core's listening endpoint.
const IPC_PATH_ENV: &str = "UNIVERSALLINK_IPC_PATH";
/// Base of the client's reconnection backoff. Barely matters: a spawn token is
/// single-use, so we exit on the first loss rather than let it retry.
const RECONNECT_BASE_DELAY: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> ExitCode {
    let Ok(ipc_path) = std::env::var(IPC_PATH_ENV) else {
        eprintln!("{IPC_PATH_ENV} is not set: the tray is launched by the Core");
        return ExitCode::FAILURE;
    };

    // Contract: the spawn token is the FIRST LINE of standard input — never
    // argv (world-readable) nor the environment (inherited by descendants).
    // Standard input then stays open; its EOF is the graceful-stop signal.
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut token = String::new();
    match stdin.read_line(&mut token).await {
        Ok(_) if !token.trim().is_empty() => {}
        _ => {
            eprintln!("no spawn token on standard input");
            return ExitCode::FAILURE;
        }
    }
    let token = token.trim().to_string();

    // Kept alive until the end of `main`: dropping the `Client` would stop the
    // connection task. Scopes: `session.read` is what the status icon will need
    // (brick 4); the grant also carries `system.shutdown` for the future Quit
    // menu, requested when that menu lands.
    let (_client, events) = universallink_ipc_client::spawn(ClientConfig {
        ipc_path: PathBuf::from(ipc_path),
        token: TokenSource::Spawn(token),
        name: "universallink-tray".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: "tray".into(),
        scopes: vec!["session.read".into()],
        topics: Vec::new(),
        reconnect_base_delay: RECONNECT_BASE_DELAY,
        request_timeout: REQUEST_TIMEOUT,
    });

    // The rest of standard input: read to EOF, which is the stop signal.
    let stdin_closed = async move {
        let mut sink = Vec::new();
        let _ = stdin.read_to_end(&mut sink).await;
    };

    match run(events, stdin_closed).await {
        Outcome::StdinClosed => ExitCode::SUCCESS,
        // IPC lost / incompatible / client ended: exit non-zero so the
        // supervisor restarts us with a fresh, single-use spawn token.
        Outcome::ConnectionLost | Outcome::Incompatible | Outcome::ClientEnded => {
            ExitCode::FAILURE
        }
    }
}
