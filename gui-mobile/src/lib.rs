// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Tauri **mobile** shell: the SAME Svelte UI and the SAME `1device-gui`
//! bridge as the desktop app, but the Core is embedded in-process (Android
//! cannot spawn a separate daemon) and reached over an in-process UDS. One UI,
//! one transport (`invoke`), no new core API — the desktop's login, enrollment
//! and devices screens run unchanged, driven by the embedded Core.
//!
//! Excluded from the workspace: built only for aarch64-linux-android via the
//! Tauri Android tooling (cargo-ndk under the hood), never by the CI jobs.

mod keepalive;
mod logcat;
mod scan;
mod share;
mod tls;

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use android_system_properties::AndroidSystemProperties;
use anyhow::Context as _;
use onedevice_core::{Config, FileSecretStore, ServerConfig};
use onedevice_daemon::dataplane::LazyIrohTransport;
use onedevice_gui::{CoreState, GUI_OPTIONAL_TOPICS, GUI_SCOPES, GUI_TOPICS, bridge_loop};
use onedevice_ipc_client::{ClientConfig, TokenSource};
use tauri::Manager;
use tls::WebPkiConnector;

/// The embedded Core, kept alive for the whole process (dropping the handle
/// stops it). Tauri owns the async runtime, so we only hold the handle.
static CORE: OnceLock<Mutex<onedevice_core::CoreHandle>> = OnceLock::new();
static LOGGING: OnceLock<()> = OnceLock::new();

/// Mobile entry point (exported to the generated Android shell via JNI).
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_logging();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            onedevice_gui::core_request,
            onedevice_gui::connection_status,
            onedevice_gui::set_server_config,
            onedevice_gui::get_server_config,
            // Mobile-only: the snapshot of the last share sheet status, and the
            // release of a shared file's cache copy (the desktop shell has no
            // share surface at all).
            share::share_status,
            share::share_taken,
            share::discard_share,
            // Mobile-only: reading a pairing code with the camera. The desktop
            // has no camera in reach of this flow, and the frontend takes the
            // absence of these commands as the answer (see `scan`).
            scan::scan_supported,
            scan::scan_code,
            // Mobile-only: work the frontend has to declare, so the process
            // survives not being the one in front — a round-trip through the
            // browser, a send waiting to be accepted (see `keepalive`).
            keepalive::keep_alive,
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
            // Before the bridge connects: what holds this process alive is read
            // off the Core's own event stream, and a transfer that started unseen
            // would never be protected (see `keepalive`).
            keepalive::watch(&handle);
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
            // logcat stamps every line it is handed, so ours would be a second
            // clock in the same line — and, more to the point, a timestamp makes
            // two otherwise identical lines different, which is exactly what the
            // repeat filter needs to compare (see `logcat`).
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .try_init();
    });
}

/// Longest model string we will pass off as a device name. A real one is short
/// ("Pixel 8", "CPH2449"); anything longer is not a model, and the devices list
/// of every other device would have to live with it.
const MAX_DEVICE_NAME: usize = 64;

/// This phone's name in the account's devices list.
///
/// `ro.product.model` IS what `android.os.Build.MODEL` reads (frameworks'
/// `Build` calls `SystemProperties.get("ro.product.model")`), and reading the
/// property store directly keeps this on the Core's boot path: a JNI call would
/// have to come FROM Kotlin, which runs after `run()` has already spawned the
/// boot — the name would then be decided by a race, on the one value the user
/// cannot correct without renaming the device.
///
/// Only enrollment sends it (core: `login.rs` `enroll`), so a phone that already
/// joined the account keeps the name it joined with; `devices.rename` is how that
/// one changes.
fn device_name() -> String {
    let model = AndroidSystemProperties::new()
        .get("ro.product.model")
        .unwrap_or_default();
    let model = model.trim();
    // A property that is missing (host build), empty, oversized or not text is
    // not a name: the fallback is honest and stable.
    let usable = !model.is_empty()
        && model.chars().count() <= MAX_DEVICE_NAME
        && !model.chars().any(char::is_control);
    if !usable {
        tracing::warn!(
            model,
            "no usable ro.product.model; naming this device Android"
        );
        return "Android".to_string();
    }
    tracing::info!(model, "device name from ro.product.model");
    model.to_string()
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

    // How the Core re-reads its config on `session.reload` (after the frontend
    // writes config.json via set_server_config): the SAME parse the daemon
    // uses. On Android there are no env overrides, so this just reads the file.
    let reload_dir = data_dir.to_path_buf();
    let reload_server: Arc<dyn Fn() -> Result<Option<ServerConfig>, String> + Send + Sync> =
        Arc::new(move || {
            let parsed = onedevice_daemon::config::load(&reload_dir);
            match parsed.problem {
                Some(problem) => Err(problem),
                None => Ok(parsed.server),
            }
        });

    // Seed the server at boot from the same parse, so a returning (already
    // configured) user connects without waiting for the frontend's first
    // session.reload. A half-filled file is logged and left unconfigured.
    let boot = onedevice_daemon::config::load(data_dir);
    if let Some(problem) = &boot.problem {
        tracing::warn!(problem = %problem, "config.json present but invalid; starting unconfigured");
    }

    // LAN discovery obeys config.json exactly like the desktop daemon (same
    // parse: default ON, a broken file fails OFF). Hearing mDNS on Android
    // additionally needs the Wi-Fi multicast filter lifted, which only Java can
    // do: LanMulticast.kt holds the `WifiManager.MulticastLock` while the app
    // has a window in front or work in flight — the rest of the time this Core
    // still announces, but is deaf to the answers (and so is every mDNS
    // listener in the process, by the platform's design, not by a defect here).
    // The configured relay rides too, exactly like the desktop daemon: the
    // phone was the one device that parsed `relay_url` and then ignored it,
    // which made "point your fleet at your relay" silently exclude Android.
    let transport = Arc::new(LazyIrohTransport::new(
        data_dir.to_path_buf(),
        boot.relay_url,
        boot.lan_discovery,
    ));

    let config = Config {
        ipc_path: ipc_path.clone(),
        config_dir: data_dir.to_path_buf(),
        server: boot.server,
        reload_server,
        device_name: device_name(),
        secret_store: Arc::new(FileSecretStore::new(data_dir)),
        connector: Arc::new(WebPkiConnector::new().context("initializing the TLS stack")?),
        transport,
        receive_dir,
        // Short on purpose: this Core's network is taken away whenever the app
        // leaves the foreground (see `keepalive`), so failed attempts are the
        // NORMAL state rather than a sign of trouble — and the backoff cap
        // derived from this (64×, ~13 s) is how long the app can look offline
        // after being opened again, or a share can wait for a session that is
        // there (share.rs `wait_ready`).
        reconnect_base_delay: Duration::from_millis(200),
    };

    let core = onedevice_core::spawn(config)
        .await
        .map_err(|e| anyhow::anyhow!("core spawn: {e}"))?;
    tracing::info!(ipc = %ipc_path.display(), "embedded Core listening");
    let _ = CORE.set(Mutex::new(core));

    Ok(ClientConfig {
        token: TokenSource::File(data_dir.join("ipc-token")),
        ipc_path,
        name: "1device-gui".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: "gui".into(),
        scopes: GUI_SCOPES.iter().map(|s| s.to_string()).collect(),
        topics: GUI_TOPICS.iter().map(|s| s.to_string()).collect(),
        optional_topics: GUI_OPTIONAL_TOPICS.iter().map(|s| s.to_string()).collect(),
        served_methods: vec![],
        reconnect_base_delay: Duration::from_secs(1),
        request_timeout: Duration::from_secs(30),
    })
}
