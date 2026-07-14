// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! `set_server_config` / `get_server_config`: the setup screen writes
//! `config.json` (the GUI is the sole writer), MERGES rather than clobbering
//! the daemon's own keys, rounds the fields back for pre-fill, and keeps the
//! file private (it may hold the OIDC secret).

use serde_json::{Value, json};

use crate::support::*;

fn read_config(shell: &Shell) -> Value {
    let text = std::fs::read_to_string(shell.config_dir().join("config.json"))
        .expect("config.json written");
    serde_json::from_str(&text).expect("config.json is JSON")
}

#[tokio::test(flavor = "multi_thread")]
async fn set_then_get_rounds_the_fields() {
    let core = TestCore::start().await;
    let shell = shell_app(gui_config(&core)).await;

    // Fresh install: nothing configured yet.
    assert_eq!(shell.get_server_config().await["server_url"], "");

    shell
        .set_server_config(json!({
            "server_url": "wss://relay.example/ws",
            "oidc_issuer": "https://idp.example",
            "oidc_client_id": "public-id",
            "oidc_client_secret": "GOCSPX-xyz",
        }))
        .await
        .expect("set_server_config");

    let after = shell.get_server_config().await;
    assert_eq!(after["server_url"], "wss://relay.example/ws");
    assert_eq!(after["oidc_issuer"], "https://idp.example");
    assert_eq!(after["oidc_client_id"], "public-id");
    assert_eq!(after["oidc_client_secret"], "GOCSPX-xyz");

    // And it landed in config.json, where the Core reads it.
    assert_eq!(read_config(&shell)["server_url"], "wss://relay.example/ws");

    // The file may carry the secret: it must be private (Unix).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(shell.config_dir().join("config.json"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "config.json must be 0600");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn set_merges_and_preserves_daemon_keys() {
    let core = TestCore::start().await;
    let shell = shell_app(gui_config(&core)).await;

    // A config.json already carrying keys the GUI does not own.
    std::fs::write(
        shell.config_dir().join("config.json"),
        json!({
            "device_name": "Living room",
            "receive_dir": "/srv/received",
            "server_url": "wss://old.example/ws",
        })
        .to_string(),
    )
    .expect("seed config.json");

    shell
        .set_server_config(json!({
            "server_url": "wss://new.example/ws",
            "oidc_issuer": "https://idp.example",
            "oidc_client_id": "id",
        }))
        .await
        .expect("set_server_config");

    let obj = read_config(&shell);
    // Server fields updated...
    assert_eq!(obj["server_url"], "wss://new.example/ws");
    assert_eq!(obj["oidc_client_id"], "id");
    // ...the daemon's own keys untouched.
    assert_eq!(obj["device_name"], "Living room");
    assert_eq!(obj["receive_dir"], "/srv/received");
    // No secret supplied → key absent (the PKCE default), not an empty string.
    assert!(obj.get("oidc_client_secret").is_none(), "{obj}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blank_secret_clears_the_key() {
    let core = TestCore::start().await;
    let shell = shell_app(gui_config(&core)).await;

    shell
        .set_server_config(json!({
            "server_url": "wss://relay.example/ws",
            "oidc_issuer": "https://idp.example",
            "oidc_client_id": "id",
            "oidc_client_secret": "GOCSPX-xyz",
        }))
        .await
        .expect("set with a secret");
    // Moving to a conformant PKCE IdP: re-saving with a blank secret removes it.
    shell
        .set_server_config(json!({
            "server_url": "wss://relay.example/ws",
            "oidc_issuer": "https://idp.example",
            "oidc_client_id": "id",
            "oidc_client_secret": "   ",
        }))
        .await
        .expect("set without a secret");

    let obj = read_config(&shell);
    assert!(
        obj.get("oidc_client_secret").is_none(),
        "a blank secret must clear the key: {obj}"
    );
}
