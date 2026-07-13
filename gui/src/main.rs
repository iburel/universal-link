// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Real GUI binary (feature `webview`): production config + system webview.
//! All the logic lives in the lib, tested on MockRuntime.

use std::time::Duration;

use universallink_ipc_client::{ClientConfig, TokenSource};

fn main() {
    #[cfg(target_os = "linux")]
    nvidia_workarounds();

    let endpoint = universallink_paths::production_endpoint()
        .expect("incomplete environment (XDG_RUNTIME_DIR / HOME / APPDATA)");

    // The Core is a per-user agent, bundled alongside the GUI: we register it
    // for subsequent sessions and launch it for this one (idempotent —
    // single-instance lock on the Core side). No privileges.
    //
    // RELEASE ONLY, via a RUNTIME guard (not a `#[cfg]`) so that CI in a debug
    // build still type-checks this block: in dev (`cargo run`), the Core is
    // launched by hand (see doc/deployment.md) — we don't install autostart
    // pointing at a `target/debug/` binary, nor spawn behind the developer's
    // back.
    if !cfg!(debug_assertions)
        && let Some(core_path) = universallink_gui::bundled_core_path()
    {
        universallink_gui::register_autostart(&core_path);
        universallink_gui::spawn_core(&core_path);
    }

    let config = ClientConfig {
        token: TokenSource::File(endpoint.token_path()),
        ipc_path: endpoint.ipc_path,
        name: "universallink-gui".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: "gui".into(),
        scopes: universallink_gui::GUI_SCOPES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        topics: universallink_gui::GUI_TOPICS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        reconnect_base_delay: Duration::from_secs(1),
        request_timeout: Duration::from_secs(30),
    };

    universallink_gui::shell(tauri::Builder::default(), config)
        .plugin(tauri_plugin_opener::init())
        .run(tauri::generate_context!())
        .expect("Tauri app startup");
}

/// webkit2gtk under the NVIDIA driver (Wayland especially): broken DMABUF
/// rendering → white window. We only force the workarounds if an NVIDIA
/// driver is present and the user hasn't decided anything themselves.
#[cfg(target_os = "linux")]
fn nvidia_workarounds() {
    if !std::path::Path::new("/proc/driver/nvidia").exists() {
        return;
    }
    for (key, value) in [
        ("WEBKIT_DISABLE_DMABUF_RENDERER", "1"),
        ("__NV_DISABLE_EXPLICIT_SYNC", "1"),
    ] {
        if std::env::var_os(key).is_none() {
            // Safe: main() is single-threaded at this stage, before any runtime.
            unsafe { std::env::set_var(key, value) };
        }
    }
}
