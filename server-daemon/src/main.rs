// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The deployable control-plane binary. It only wires things up: logging,
//! reading the environment configuration, loading the persisted directory,
//! starting the server, shutting down on signal. All the logic lives in
//! `universallink-server`.
//!
//! Accepted limitation at this stage (follow-up building block): TLS is
//! terminated upstream (reverse proxy) — the server listens in the clear on its
//! internal network.

use std::process::ExitCode;
use std::sync::Arc;

use universallink_server_daemon::config;
use universallink_server_daemon::store::FileStore;

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();

    // Invalid config: we refuse to start. A server has no UI to say what's
    // wrong; it logs it and exits.
    let config = match config::load() {
        Ok(config) => config,
        Err(reason) => {
            tracing::error!(%reason, "invalid configuration: startup refused");
            return ExitCode::FAILURE;
        }
    };

    let state_path = config::state_path();
    tracing::info!(directory = %state_path.display(), "persisted directory");
    let store = Arc::new(FileStore::new(state_path));

    let mut server = match universallink_server::spawn_with_store(config, store).await {
        Ok(server) => server,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "could not start listening");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(addr = %server.local_addr(), "server listening");

    // `wait()` only returns if the axum task stops on its own (error): this is
    // how we tell a REQUESTED shutdown (signal, exit 0) from a CRASHED server
    // (exit 1, so an orchestrator restarts it).
    tokio::select! {
        _ = shutdown_signal() => {
            tracing::info!("shutdown requested");
            ExitCode::SUCCESS
        }
        _ = server.wait() => {
            tracing::error!("the server stopped on its own");
            ExitCode::FAILURE
        }
    }
    // `server` is dropped here: its axum task is aborted. In-flight connections
    // are cut off abruptly — acceptable for a control plane (clients reconnect).
    // A graceful axum shutdown is a follow-up building block.
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;

    // `UNIVERSALLINK_LOG` (not `RUST_LOG`, too shared), like the Core.
    // Output on stderr: a container collects the standard stream.
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .with_env_var("UNIVERSALLINK_LOG")
        .from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Waits until we are asked to leave. `SIGTERM` is the shutdown signal for
/// containers and service managers; `Ctrl-C` for launching by hand.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

/// The deployment target is unix; Windows is only there so the crate compiles
/// on all three CI runners.
#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
