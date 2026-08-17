// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Real GUI binary (feature `webview`): production config + system webview.
//! All the logic lives in the lib, tested on MockRuntime.

use std::time::Duration;

use onedevice_ipc_client::{ClientConfig, TokenSource};

fn main() {
    #[cfg(target_os = "linux")]
    nvidia_workarounds();

    let endpoint = onedevice_paths::production_endpoint()
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
        && let Some(bundled) = onedevice_gui::bundled_core_path()
    {
        // On Linux/AppImage the bundled path is an ephemeral mount: stabilize
        // it to a durable per-user copy so autostart survives logout. A no-op
        // elsewhere (and outside an AppImage).
        let core_path = onedevice_gui::stabilize_core_path(&bundled);
        onedevice_gui::register_autostart(&core_path);
        onedevice_gui::spawn_core(&core_path);
        // Record how the tray should relaunch us: it runs from the Core's
        // durable copy and cannot otherwise find the GUI (a loose AppImage on
        // Linux).
        onedevice_gui::record_launch_target(&endpoint.gui_launch_path());
    }

    let config = ClientConfig {
        token: TokenSource::File(endpoint.token_path()),
        ipc_path: endpoint.ipc_path,
        name: "1device-gui".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: "gui".into(),
        scopes: onedevice_gui::GUI_SCOPES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        topics: onedevice_gui::GUI_TOPICS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        // The Input tab's surface is asked for optionally, so a Core that
        // predates it costs the tab and not the connection (see the constants).
        optional_scopes: onedevice_gui::GUI_INPUT_SCOPES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        optional_topics: onedevice_gui::GUI_OPTIONAL_TOPICS
            .iter()
            .chain(onedevice_gui::GUI_INPUT_TOPICS.iter())
            .map(|s| s.to_string())
            .collect(),
        served_methods: vec![],
        reconnect_base_delay: Duration::from_secs(1),
        request_timeout: Duration::from_secs(30),
    };

    // A second launch hands off to the running instance — which surfaces its
    // window — and exits, instead of opening a second window on the same Core.
    // Registered on the incoming builder so it is the FIRST plugin: plugins
    // initialize in registration order, and everything after this line is
    // wasted work in a process about to defer. (The supervise block above does
    // run twice; it is idempotent by design — the Core holds its own lock.)
    // This is also what the tray's "Open" relies on when the window is already
    // there: it just spawns this binary, and the handoff turns that into a
    // focus.
    let first_instance =
        tauri::Builder::default().plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            use tauri::Manager;
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }));

    onedevice_gui::shell(first_instance, config, endpoint.config_dir)
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
