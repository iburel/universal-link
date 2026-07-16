// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Core binary. All it does is wire things together: production paths,
//! config, keyring, TLS, log, supervisor, signals. All the logic lives in the
//! daemon lib and in `universallink-core`.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use universallink_daemon::supervisor::{Policy, Supervisor};
use universallink_daemon::{config, dataplane, logging, secrets, supervisor, tls};

/// What we allow ourselves to stop the children and close the IPC. Windows
/// grants only about five seconds after a CTRL_CLOSE_EVENT; beyond that it
/// terminates without warning.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);
/// Base of the server reconnection backoff.
const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(1);

enum Outcome {
    Stopped,
    /// This is not a failure: a Core is already listening for this user.
    AlreadyRunning,
}

#[tokio::main]
async fn main() -> ExitCode {
    // The guard lives until the return of `main`: it is its `drop` that
    // flushes the log buffer.
    let _guard = logging::init();
    match run().await {
        Ok(Outcome::Stopped) => {
            tracing::info!("Core stopped");
            ExitCode::SUCCESS
        }
        // Exit 0: an autostart that races a manual launch must not appear as a
        // failed service.
        Ok(Outcome::AlreadyRunning) => {
            tracing::info!("a Core is already running for this user: nothing to do");
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "startup failed");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<Outcome> {
    let endpoint = universallink_paths::production_endpoint()
        .context("incomplete environment (XDG_RUNTIME_DIR / HOME / APPDATA)")?;
    std::fs::create_dir_all(&endpoint.config_dir).with_context(|| {
        format!(
            "creating the config directory {}",
            endpoint.config_dir.display()
        )
    })?;

    let settings = config::load(&endpoint.config_dir);
    if let Some(problem) = &settings.problem {
        // We start anyway: the IPC is the only channel through which the GUI
        // can tell the user what is wrong.
        tracing::error!(%problem, "unusable configuration: Core started unconfigured");
    } else if settings.server.is_none() {
        tracing::info!("Core not configured yet: the GUI's setup screen will write config.json");
    }

    let secrets = secrets::build(&endpoint.config_dir);
    tracing::info!(secrets = secrets.description(), "keyring chosen");

    let connector = Arc::new(tls::TlsConnector::new().context("initializing the TLS stack")?);

    // LAZILY-bound data plane: the iroh endpoint (and the read of
    // `device.key`, which seeds it) only exist on the first use — triggered by
    // session establishment, hence AFTER the Core's instance lock and only if
    // the device is enrolled. A never-configured Core emits no iroh traffic,
    // and a bind failure does not deprive the user of the IPC (it is logged
    // and retried).
    let transport = Arc::new(dataplane::LazyIrohTransport::new(
        endpoint.config_dir.clone(),
        settings.relay_url,
    ));

    // How the Core re-reads its config on `session.reload` (once the GUI has
    // written config.json): the SAME parse as startup — env still overrides,
    // and a half-filled file surfaces its reason rather than reverting to
    // unconfigured. The daemon owns this parsing; the Core only calls back in.
    let reload_dir = endpoint.config_dir.clone();
    let reload_server: Arc<
        dyn Fn() -> Result<Option<universallink_core::ServerConfig>, String> + Send + Sync,
    > = Arc::new(move || {
        let parsed = config::load(&reload_dir);
        match parsed.problem {
            Some(problem) => Err(problem),
            None => Ok(parsed.server),
        }
    });

    let core = universallink_core::spawn(universallink_core::Config {
        ipc_path: endpoint.ipc_path.clone(),
        config_dir: endpoint.config_dir.clone(),
        server: settings.server,
        reload_server,
        device_name: settings.device_name,
        secret_store: secrets.store(),
        connector,
        transport: transport.clone(),
        receive_dir: settings.receive_dir,
        reconnect_base_delay: RECONNECT_BASE_DELAY,
    })
    .await;
    let core = match core {
        Ok(core) => core,
        Err(universallink_core::SpawnError::AlreadyRunning) => {
            secrets.flush();
            return Ok(Outcome::AlreadyRunning);
        }
        Err(universallink_core::SpawnError::Failed(e)) => {
            secrets.flush();
            return Err(e.context("starting the Core"));
        }
    };
    tracing::info!(ipc = %endpoint.ipc_path.display(), "Core listening");

    let core = Arc::new(core);
    let supervisor = Supervisor::start(
        core.clone(),
        endpoint.ipc_path,
        supervisor::official_components(),
        Policy::default(),
    );

    // Two ways out: an OS signal (service stop, session end) or a component
    // over the IPC (the tray's Quit → `system.shutdown`). The teardown below is
    // the same for both.
    tokio::select! {
        _ = shutdown_signal() => tracing::info!("shutdown requested (signal)"),
        _ = core.shutdown_requested() => tracing::info!("shutdown requested (component)"),
    }
    // A second signal during shutdown: the user insists.
    tokio::spawn(async {
        shutdown_signal().await;
        tracing::warn!("second signal: immediate exit");
        std::process::exit(1);
    });

    // Order matters. The children are stopped and REAPED while the tokio
    // runtime is still alive — otherwise they become zombies. Only then does
    // the Core close its IPC connections and release the instance lock.
    if tokio::time::timeout(SHUTDOWN_BUDGET, supervisor.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("stopping the components is taking too long: leaving without waiting for them");
    }
    drop(core);
    // The iroh endpoint closes AFTER the Core (nobody opens streams anymore):
    // the peers are notified instead of waiting for a timeout. Bounded
    // internally.
    universallink_core::PeerTransport::close(transport.as_ref()).await;
    secrets.flush();
    Ok(Outcome::Stopped)
}

/// Waits until we are asked to leave.
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM");
    // SIGHUP means shutdown, not reload: we have nothing to reload, and the
    // default behavior (dying without warning) would abandon the components
    // behind us.
    let mut hangup = signal(SignalKind::hangup()).expect("SIGHUP");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
        _ = hangup.recv() => {}
    }
}

/// With no console attached, none of these events arrive: a Core started by a
/// graphical autostart will not know the session is being closed. The
/// packaging building block will have to give it a message-only window
/// (`WM_QUERYENDSESSION`) or make it a real service.
#[cfg(windows)]
async fn shutdown_signal() {
    use tokio::signal::windows;

    let mut close = windows::ctrl_close().expect("ctrl_close");
    let mut shutdown = windows::ctrl_shutdown().expect("ctrl_shutdown");
    let mut logoff = windows::ctrl_logoff().expect("ctrl_logoff");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = close.recv() => {}
        _ = shutdown.recv() => {}
        _ = logoff.recv() => {}
    }
}
