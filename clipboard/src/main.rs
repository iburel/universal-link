// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The `universallink-clipboard` binary: the platform clipboard OS loop on the
//! main thread (X11 pins its non-`Send` connection there; Windows pins its
//! message-only window and pump there), plus the async IPC brain (a tokio
//! runtime on a side thread) running the OS-agnostic orchestrator. The two are
//! bridged by the `Clone` backend handle (downcalls + `request_exit`) and a
//! `BackendEvent` channel (upcalls). Mirrors the `universallink-tray` shape; all
//! the testable logic lives in the lib.
//!
//! Supervised-component contract (see `daemon/src/supervisor.rs`): find the
//! Core at `UNIVERSALLINK_IPC_PATH`, read the single-use spawn token from the
//! first line of standard input, keep that standard input open (its EOF means
//! "stop"), and exit if the IPC connection drops (the token is single-use, so a
//! reconnection would fail — exiting lets the supervisor restart us fresh).

use std::process::ExitCode;

use universallink_clipboard::os;

fn main() -> ExitCode {
    match os::create() {
        Err(os::Unsupported) => {
            eprintln!(
                "universallink-clipboard: no clipboard backend for this platform \
                 (or no X server); nothing to run."
            );
            ExitCode::SUCCESS
        }
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        Ok(created) => {
            let handle = created.handle;
            let backend_events = created.backend_events;
            let event_loop = created.event_loop;

            // The async brain on its own thread with its own tokio runtime; it
            // owns a clone of the handle (the backend the orchestrator drives),
            // and when it returns, asks the main-thread loop to exit with the
            // mapped code.
            let brain_backend = handle.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                let code = runtime.block_on(brain(brain_backend, backend_events));
                handle.request_exit(code);
            });

            // The OS event loop pumps on THIS (main) thread until the brain
            // requests an exit; its return value is the process exit code.
            let code = event_loop.run();
            std::process::exit(code);
        }
        // Platforms without a backend yet: `create()` returns
        // `Result<Infallible, _>`, so the `Ok` arm is uninhabited.
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        Ok(never) => match never {},
    }
}

/// Reads the token and environment, connects, and runs the orchestrator.
/// Returns the process exit code. Generic over the backend so it never has to
/// name the platform handle type (which lives in a private module).
#[cfg(any(target_os = "linux", target_os = "windows"))]
async fn brain<B: universallink_clipboard::ClipboardBackend>(
    backend: B,
    backend_events: tokio::sync::mpsc::Receiver<universallink_clipboard::BackendEvent>,
) -> i32 {
    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use universallink_clipboard::{Outcome, run};
    use universallink_ipc_client::{ClientConfig, TokenSource};

    const IPC_PATH_ENV: &str = "UNIVERSALLINK_IPC_PATH";

    let Ok(ipc_path) = std::env::var(IPC_PATH_ENV) else {
        eprintln!("{IPC_PATH_ENV} is not set: the clipboard backend is launched by the Core");
        return 1;
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
            return 1;
        }
    }
    let token = token.trim().to_string();

    let (client, events) = universallink_ipc_client::spawn(ClientConfig {
        ipc_path: PathBuf::from(&ipc_path),
        token: TokenSource::Spawn(token),
        name: "universallink-clipboard".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: "clipboard-backend".into(),
        scopes: vec![
            "devices.read".into(),
            "clipboard.read".into(),
            "clipboard.write".into(),
        ],
        topics: vec!["clipboard".into()],
        served_methods: vec!["clipboard.get_data".into()],
        reconnect_base_delay: Duration::from_millis(500),
        request_timeout: Duration::from_secs(10),
    });

    // The rest of standard input: reading it to EOF is the stop signal.
    let stdin_closed = async move {
        let mut sink = Vec::new();
        let _ = stdin.read_to_end(&mut sink).await;
    };

    match run(
        client,
        events,
        backend,
        PathBuf::from(ipc_path),
        backend_events,
        stdin_closed,
    )
    .await
    {
        Outcome::StdinClosed => 0,
        // IPC lost / incompatible / client or backend ended: exit non-zero so
        // the supervisor restarts us with a fresh, single-use spawn token.
        _ => 1,
    }
}
