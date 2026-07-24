// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Android native shim (brick 1: on-device smoke test).
//!
//! `NativeCore.nativeStart(dataDir)` boots the embedded Core in the app
//! process — the same wiring as the desktop daemon's `main.rs`, minus the
//! supervisor and the OS signals. The Core binds its UDS at
//! `<dataDir>/core.sock`; the Kotlin app connects to it as a component and
//! speaks the existing core-api (it does the `hello` handshake itself).
//!
//! Beyond just starting, this shim runs two on-device checks that de-risk the
//! whole 4th-client idea, using the very trait objects `main.rs` holds — no new
//! core API:
//!   - **iroh**: force the endpoint to bind and reach a relay
//!     (`PeerTransport::home_relay`). This is the real unknown — iroh's
//!     interface enumeration uses netlink, restricted for apps on Android 11+.
//!   - **TLS egress**: a handshake to the real server over 443 with bundled
//!     roots (the OS-trust-store path via rustls-platform-verifier is wired in
//!     the login brick, where a full session is actually established).
//!
//! The result of both is returned to Kotlin as a human-readable summary and
//! shown on screen.

mod logcat;

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context as _;
use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::jstring;

use universallink_core::{Config, FileSecretStore, PeerTransport, PlainConnector, ServerConfig};
use universallink_daemon::dataplane::LazyIrohTransport;

/// The real deployment (for the TLS reachability probe).
const SERVER_HOST: &str = "universallink.biou-server.com";

/// Kept alive for the whole process: dropping the `CoreHandle` stops the Core,
/// and dropping the runtime would kill its tasks. Stored once, never taken out.
struct Running {
    _runtime: tokio::runtime::Runtime,
    _core: universallink_core::CoreHandle,
}

static RUNNING: OnceLock<Mutex<Running>> = OnceLock::new();
static LOGGING: OnceLock<()> = OnceLock::new();

/// JNI entry: `dev.universallink.app.NativeCore.nativeStart(String): String`.
///
/// # Safety
/// Called by the JVM with a valid env and a UTF-8 `data_dir` string.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_universallink_app_NativeCore_nativeStart<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    data_dir: JString<'local>,
) -> jstring {
    let data_dir: String = match env.get_string(&data_dir) {
        Ok(s) => s.into(),
        Err(e) => return new_string(&mut env, &format!("bad data_dir: {e}")),
    };

    // A panic must never unwind across the FFI boundary (UB): contain it.
    let summary = std::panic::catch_unwind(|| start(PathBuf::from(data_dir)))
        .unwrap_or_else(|_| "panic in native start (see logcat)".to_string());
    logcat::line(&summary);
    new_string(&mut env, &summary)
}

fn new_string(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s)
        .map(|o| o.into_raw())
        .unwrap_or(std::ptr::null_mut())
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

fn start(data_dir: PathBuf) -> String {
    init_logging();
    if RUNNING.get().is_some() {
        return "core already running".to_string();
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return format!("runtime build failed: {e}"),
    };

    match runtime.block_on(boot(data_dir)) {
        Ok((core, summary)) => {
            let _ = RUNNING.set(Mutex::new(Running {
                _runtime: runtime,
                _core: core,
            }));
            summary
        }
        Err(e) => format!("core boot failed: {e:#}"),
    }
}

/// Wires and spawns the Core, then runs the two de-risking checks. Returns the
/// live handle (to keep) and a human summary.
async fn boot(data_dir: PathBuf) -> anyhow::Result<(universallink_core::CoreHandle, String)> {
    std::fs::create_dir_all(&data_dir).context("creating the data dir")?;
    let ipc_path = data_dir.join("core.sock");
    // A socket file left by a previous run would make bind fail with EADDRINUSE
    // even though no Core is listening: clear it (the process holding it, if
    // any, is gone with the app).
    let _ = std::fs::remove_file(&ipc_path);
    let receive_dir = data_dir.join("received");

    let transport = std::sync::Arc::new(LazyIrohTransport::new(data_dir.clone(), None));
    let reload_server: std::sync::Arc<
        dyn Fn() -> Result<Option<ServerConfig>, String> + Send + Sync,
    > = std::sync::Arc::new(|| Ok(None));

    let config = Config {
        ipc_path: ipc_path.clone(),
        config_dir: data_dir.clone(),
        // Brick 1 does not connect to the server (no session / enrollment yet).
        server: None,
        reload_server,
        device_name: "android-smoke".to_string(),
        secret_store: std::sync::Arc::new(FileSecretStore::new(&data_dir)),
        // Never used in brick 1 (no server session). The TLS connector proper
        // (OS trust store) arrives with the login brick.
        connector: std::sync::Arc::new(PlainConnector),
        transport: transport.clone(),
        receive_dir,
        reconnect_base_delay: Duration::from_secs(1),
    };

    let core = universallink_core::spawn(config)
        .await
        .map_err(|e| anyhow::anyhow!("spawn: {e}"))?;
    tracing::info!(ipc = %ipc_path.display(), "embedded Core listening");

    // Risk A — force the iroh endpoint to bind and try to reach a relay. On a
    // desktop this happens at session establishment; here we trigger it
    // directly so the smoke test exercises it without enrollment.
    let relay = PeerTransport::home_relay(transport.as_ref()).await;
    let iroh_line = match &relay {
        Some(url) => format!("iroh: BOUND, relay {url}"),
        None => "iroh: bound but no relay reached (offline / relay unreachable)".to_string(),
    };
    tracing::info!(%iroh_line);

    // Risk B — plain TLS reachability of the real server from the app.
    let tls_line = match probe_tls(SERVER_HOST, 443).await {
        Ok(()) => format!("tls: OK to {SERVER_HOST}:443"),
        Err(e) => format!("tls: FAILED to {SERVER_HOST}:443 — {e}"),
    };
    tracing::info!(%tls_line);

    let summary = format!(
        "core: OK\n  ipc={}\n{iroh_line}\n{tls_line}",
        ipc_path.display()
    );
    Ok((core, summary))
}

/// Opens a TLS connection to `host:port` with bundled Mozilla roots and lets
/// the handshake complete. Proves egress + a working TLS stack on device; it
/// deliberately does NOT use the OS trust store (that plumbing belongs with the
/// real server session).
async fn probe_tls(host: &str, port: u16) -> Result<(), String> {
    use std::sync::Arc;

    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};

    // The ring provider must be installed before building a config.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let tcp = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    .map_err(|_| "tcp connect timed out".to_string())?
    .map_err(|e| format!("tcp: {e}"))?;

    let name = ServerName::try_from(host.to_string()).map_err(|e| format!("bad name: {e}"))?;
    tokio::time::timeout(Duration::from_secs(10), connector.connect(name, tcp))
        .await
        .map_err(|_| "tls handshake timed out".to_string())?
        .map_err(|e| format!("handshake: {e}"))?;
    Ok(())
}
