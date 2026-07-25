// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Tauri **mobile** shell: the SAME Svelte UI and the SAME `universallink-gui`
//! bridge as the desktop app, but the Core is embedded in-process (Android
//! cannot spawn a separate daemon) and reached over an in-process UDS. One UI,
//! one transport (`invoke`), no new core API — the desktop's login, enrollment
//! and devices screens run unchanged, driven by the embedded Core.
//!
//! Excluded from the workspace: built only for aarch64-linux-android via the
//! Tauri Android tooling (cargo-ndk under the hood), never by the CI jobs.

mod logcat;
mod share;
mod tls;

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context as _;
use tauri::Manager;
use tls::WebPkiConnector;
use universallink_core::{Config, FileSecretStore, ServerConfig};
use universallink_daemon::dataplane::LazyIrohTransport;
use universallink_gui::{CoreState, GUI_SCOPES, GUI_TOPICS, bridge_loop};
use universallink_ipc_client::{ClientConfig, TokenSource};

/// The embedded Core, kept alive for the whole process (dropping the handle
/// stops it). Tauri owns the async runtime, so we only hold the handle.
static CORE: OnceLock<Mutex<universallink_core::CoreHandle>> = OnceLock::new();
static LOGGING: OnceLock<()> = OnceLock::new();

/// Mobile entry point (exported to the generated Android shell via JNI).
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_logging();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            universallink_gui::core_request,
            universallink_gui::connection_status,
            universallink_gui::set_server_config,
            universallink_gui::get_server_config,
            // Mobile-only: the snapshot of the last share sheet status, and the
            // release of a shared file's cache copy (the desktop shell has no
            // share surface at all).
            share::share_status,
            share::share_taken,
            share::discard_share,
        ])
        .setup(|app| {
            // The app-private data dir is only resolvable here (it needs the
            // Android Context). It is the single directory the bridge writes
            // config.json into (`set_server_config`) and the embedded Core
            // reads it back from (`session.reload`) — the mobile analogue of
            // the desktop's shared `endpoint.config_dir`.
            let data_dir = app
                .path()
                .app_data_dir()
                .context("resolving the app data dir")?;
            // `write_private` (set_server_config) does not create parents, and
            // the Core's boot does the same create in the async task; do it
            // synchronously here so the config write cannot race the dir.
            std::fs::create_dir_all(&data_dir).context("creating the app data dir")?;

            // Manage the bridge state with the REAL config_dir BEFORE spawning
            // the bridge loop: `bridge_loop` and every `invoke` resolve
            // `State<CoreState>` at call time and panic if it is not managed.
            app.manage(CoreState::new(data_dir.clone()));

            let handle = app.handle().clone();
            // Boot the embedded Core, then run the same bridge loop the desktop
            // uses — pointed at the in-process socket instead of the daemon's.
            tauri::async_runtime::spawn(async move {
                match boot_core(&data_dir).await {
                    Ok(client_config) => {
                        // The share sheet's own connection to the Core (it needs
                        // the announcing role the bridge does not have). Started
                        // from the bridge's config so both reach the same socket
                        // with the same token — see `share`.
                        share::spawn(handle.clone(), &client_config);
                        bridge_loop(handle, client_config).await
                    }
                    Err(e) => tracing::error!("embedded Core boot failed: {e:#}"),
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the mobile app");
}

fn init_logging() {
    LOGGING.get_or_init(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(logcat::MakeLogcat)
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .try_init();
    });
}

/// Spawns the embedded Core and returns the client config the bridge uses to
/// reach it over the in-process socket. Mirrors the desktop daemon's wiring:
/// real TLS connector, lazy iroh transport, file secret store, and a
/// `reload_server` that re-reads `config.json` on `session.reload`.
async fn boot_core(data_dir: &Path) -> anyhow::Result<ClientConfig> {
    std::fs::create_dir_all(data_dir).context("creating the data dir")?;
    let ipc_path = data_dir.join("core.sock");
    // A socket file left by a previous run would make bind fail with EADDRINUSE
    // even though no Core is listening: clear it.
    let _ = std::fs::remove_file(&ipc_path);
    let receive_dir = data_dir.join("received");

    let transport = Arc::new(LazyIrohTransport::new(data_dir.to_path_buf(), None));

    // How the Core re-reads its config on `session.reload` (after the frontend
    // writes config.json via set_server_config): the SAME parse the daemon
    // uses. On Android there are no env overrides, so this just reads the file.
    let reload_dir = data_dir.to_path_buf();
    let reload_server: Arc<dyn Fn() -> Result<Option<ServerConfig>, String> + Send + Sync> =
        Arc::new(move || {
            let parsed = universallink_daemon::config::load(&reload_dir);
            match parsed.problem {
                Some(problem) => Err(problem),
                None => Ok(parsed.server),
            }
        });

    // Seed the server at boot from the same parse, so a returning (already
    // configured) user connects without waiting for the frontend's first
    // session.reload. A half-filled file is logged and left unconfigured.
    let boot = universallink_daemon::config::load(data_dir);
    if let Some(problem) = &boot.problem {
        tracing::warn!(problem = %problem, "config.json present but invalid; starting unconfigured");
    }

    let config = Config {
        ipc_path: ipc_path.clone(),
        config_dir: data_dir.to_path_buf(),
        server: boot.server,
        reload_server,
        // TODO(brick 6): derive from android.os.Build.MODEL so a user's phone
        // shows a recognizable name in the devices list.
        device_name: "Android".to_string(),
        secret_store: Arc::new(FileSecretStore::new(data_dir)),
        connector: Arc::new(WebPkiConnector::new().context("initializing the TLS stack")?),
        transport,
        receive_dir,
        reconnect_base_delay: Duration::from_secs(1),
    };

    let core = universallink_core::spawn(config)
        .await
        .map_err(|e| anyhow::anyhow!("core spawn: {e}"))?;
    tracing::info!(ipc = %ipc_path.display(), "embedded Core listening");
    let _ = CORE.set(Mutex::new(core));

    Ok(ClientConfig {
        token: TokenSource::File(data_dir.join("ipc-token")),
        ipc_path,
        name: "universallink-gui".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: "gui".into(),
        scopes: GUI_SCOPES.iter().map(|s| s.to_string()).collect(),
        topics: GUI_TOPICS.iter().map(|s| s.to_string()).collect(),
        served_methods: vec![],
        reconnect_base_delay: Duration::from_secs(1),
        request_timeout: Duration::from_secs(30),
    })
}
