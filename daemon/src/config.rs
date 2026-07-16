// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! What the daemon needs to know to talk to the deployment: the server URL and
//! the IdP that authenticates it. Nothing secret (the OIDC client is public).
//!
//! Source: `config.json` in the config directory, written by the GUI's setup
//! screen — the daemon only ever READS it, never rewrites it. The
//! `UNIVERSALLINK_*` environment variables override it, for development.
//! NOTHING is baked into the binary: a fresh install carries no server, and the
//! GUI walks the user through configuring one, then has the Core re-read this
//! file live (see `session.reload`). Precedence: env > `config.json`.
//!
//! Two states, one behavior: **the Core always starts**. With nothing
//! configured it runs unlinked (`session.login` answers `SERVER_UNREACHABLE`,
//! and `session.status` reports `configured: false`) — exactly what is needed:
//! the IPC is the only channel through which the GUI can tell the user what to
//! do. Refusing to start would leave them staring at an eternal "Connecting to
//! the Core…" screen. A PARTIAL configuration (some fields but not all three)
//! stays a fault, surfaced as such — never silently ignored.

use std::path::{Path, PathBuf};

use universallink_core::ServerConfig;

/// The file, as we read it — every field optional, so that the environment can
/// complete a partial file. Validating completeness BEFORE the merge would
/// make the announced precedence a lie.
#[derive(Default)]
struct Fields {
    server_url: Option<String>,
    oidc_issuer: Option<String>,
    oidc_client_id: Option<String>,
    /// Optional: most IdPs (PKCE) do not have one. Google requires it even in
    /// PKCE. Its absence is NEVER a config fault.
    oidc_client_secret: Option<String>,
    device_name: Option<String>,
    relay_url: Option<String>,
    receive_dir: Option<String>,
}

pub struct DaemonConfig {
    /// `None`: Core not configured. It starts anyway.
    pub server: Option<ServerConfig>,
    pub device_name: String,
    /// The deployment's iroh relay (self-hosted). `None`: the n0 public
    /// relays — a deployment that hosts its own server can also host its own
    /// relay, without depending on third-party infra for its data plane.
    pub relay_url: Option<iroh::RelayUrl>,
    /// Where received files land. Always set — the Core receives even without
    /// `config.json`: the configured directory, otherwise the user's
    /// downloads, otherwise (silent environment) the config directory.
    pub receive_dir: PathBuf,
    /// Configuration present but unusable. The daemon logs it and starts
    /// unconfigured: a faulty `config.json` must not deprive the user of their
    /// interface.
    pub problem: Option<String>,
}

pub fn load(config_dir: &Path) -> DaemonConfig {
    load_from(config_dir, &|key| std::env::var(key).ok(), hostname)
}

fn load_from(
    config_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
    fallback_name: impl FnOnce() -> String,
) -> DaemonConfig {
    // File present but unreadable: the environment does not "repair" a broken
    // file, it overrides values. We give up on the server — but not on the
    // rest: the Core must start, and for that it needs a device name.
    let (mut fields, mut problem) = match read_file(&config_dir.join("config.json")) {
        Ok(fields) => (fields, None),
        Err(problem) => (Fields::default(), Some(problem)),
    };

    // A variable that is SET BUT EMPTY overrides nothing: `export FOO=` in a
    // script must not erase the file.
    let over = |key: &str, field: &mut Option<String>| {
        if let Some(value) = env(key).filter(|v| !v.trim().is_empty()) {
            *field = Some(value);
        }
    };
    over("UNIVERSALLINK_DEVICE_NAME", &mut fields.device_name);
    over("UNIVERSALLINK_RECEIVE_DIR", &mut fields.receive_dir);
    let device_name = fields
        .device_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(fallback_name);
    // Always resolved (the Core receives even with broken / absent config).
    let receive_dir = resolve_receive_dir(fields.receive_dir.as_deref(), config_dir, env);

    if problem.is_some() {
        return DaemonConfig {
            server: None,
            device_name,
            relay_url: None,
            receive_dir,
            problem,
        };
    }

    over("UNIVERSALLINK_SERVER_URL", &mut fields.server_url);
    over("UNIVERSALLINK_OIDC_ISSUER", &mut fields.oidc_issuer);
    over("UNIVERSALLINK_OIDC_CLIENT_ID", &mut fields.oidc_client_id);
    over(
        "UNIVERSALLINK_OIDC_CLIENT_SECRET",
        &mut fields.oidc_client_secret,
    );
    over("UNIVERSALLINK_RELAY_URL", &mut fields.relay_url);

    let server = match validate(&fields) {
        Ok(Some(server)) => Some(server),
        // Nothing configured: the Core starts unlinked and the GUI offers its
        // setup screen. A PARTIAL config, by contrast, was already rejected by
        // `validate` as `Err` below.
        Ok(None) => None,
        Err(reason) => {
            problem = Some(reason);
            None
        }
    };
    // The relay is checked at startup like the server URLs: a typo would
    // otherwise give a silent data plane, with no explanation.
    let relay_url = match fields.relay_url.as_deref().map(str::parse) {
        None => None,
        Some(Ok(url)) => Some(url),
        Some(Err(e)) => {
            problem.get_or_insert(format!("relay_url is not a valid relay URL: {e}"));
            None
        }
    };
    DaemonConfig {
        server,
        device_name,
        relay_url,
        receive_dir,
        problem,
    }
}

/// Where to drop received files. Priority: configured value (`config.json` or
/// `UNIVERSALLINK_RECEIVE_DIR`) > `<Downloads>/UniversalLink` >
/// `<config directory>/received` (last resort, always available). The
/// directory itself is created on the first incoming transfer, by the Core.
fn resolve_receive_dir(
    configured: Option<&str>,
    config_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> PathBuf {
    if let Some(dir) = configured.map(str::trim).filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(downloads) = download_dir(env) {
        return downloads.join("UniversalLink");
    }
    config_dir.join("received")
}

/// The user's downloads directory, if it can be determined.
#[cfg(target_os = "linux")]
fn download_dir(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let abs = |key: &str| env(key).filter(|v| v.starts_with('/')).map(PathBuf::from);
    // XDG_DOWNLOAD_DIR (user-dirs) wins, otherwise ~/Downloads.
    abs("XDG_DOWNLOAD_DIR").or_else(|| abs("HOME").map(|home| home.join("Downloads")))
}

#[cfg(target_os = "macos")]
fn download_dir(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("HOME")
        .filter(|v| v.starts_with('/'))
        .map(|home| PathBuf::from(home).join("Downloads"))
}

#[cfg(windows)]
fn download_dir(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("USERPROFILE")
        .filter(|v| !v.is_empty())
        .map(|profile| PathBuf::from(profile).join("Downloads"))
}

/// `Ok(None)`: nothing is set, the Core has never been configured.
/// `Err`: half set, or set in a nonsensical way.
fn validate(fields: &Fields) -> Result<Option<ServerConfig>, String> {
    let present: Vec<&str> = [
        ("server_url", &fields.server_url),
        ("oidc_issuer", &fields.oidc_issuer),
        ("oidc_client_id", &fields.oidc_client_id),
    ]
    .iter()
    .filter(|(_, value)| value.is_some())
    .map(|(name, _)| *name)
    .collect();
    if present.is_empty() {
        return Ok(None);
    }
    let (Some(url), Some(oidc_issuer), Some(oidc_client_id)) = (
        fields.server_url.clone(),
        fields.oidc_issuer.clone(),
        fields.oidc_client_id.clone(),
    ) else {
        return Err(format!(
            "incomplete configuration: only {} are set (server_url, oidc_issuer and oidc_client_id are required)",
            present.join(", ")
        ));
    };
    // The scheme is checked here rather than discovered on the first
    // connection: a typo would otherwise give a `SERVER_UNREACHABLE` with no
    // explanation.
    if !(url.starts_with("ws://") || url.starts_with("wss://")) {
        return Err(format!("server_url must start with ws:// or wss://: {url}"));
    }
    if !(oidc_issuer.starts_with("http://") || oidc_issuer.starts_with("https://")) {
        return Err(format!(
            "oidc_issuer must start with http:// or https://: {oidc_issuer}"
        ));
    }
    Ok(Some(ServerConfig {
        url,
        oidc_issuer,
        oidc_client_id,
        // Optional: copied as-is (absent for a conformant PKCE IdP).
        oidc_client_secret: fields.oidc_client_secret.clone(),
    }))
}

/// File absent: empty `Fields`, not an error.
fn read_file(path: &Path) -> Result<Fields, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Fields::default()),
        Err(e) => return Err(format!("{} is unreadable: {e}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    let Some(object) = value.as_object() else {
        return Err(format!("{} must contain a JSON object", path.display()));
    };
    let mut fields = Fields::default();
    for (key, slot) in [
        ("server_url", &mut fields.server_url),
        ("oidc_issuer", &mut fields.oidc_issuer),
        ("oidc_client_id", &mut fields.oidc_client_id),
        ("oidc_client_secret", &mut fields.oidc_client_secret),
        ("device_name", &mut fields.device_name),
        ("relay_url", &mut fields.relay_url),
        ("receive_dir", &mut fields.receive_dir),
    ] {
        match object.get(key) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::String(text)) => *slot = Some(text.clone()),
            Some(_) => {
                return Err(format!("{key} must be a string in {}", path.display()));
            }
        }
    }
    Ok(fields)
}

/// The device's name in the directory: a display label, not an identity —
/// that is the device's public key. Two machines can bear the same hostname
/// with no consequence.
fn hostname() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    if name.trim().is_empty() {
        "unnamed-device".to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    fn write(dir: &tempfile::TempDir, text: &str) {
        std::fs::write(dir.path().join("config.json"), text).expect("write config.json");
    }

    fn load_with(dir: &tempfile::TempDir, vars: &[(&str, &str)]) -> DaemonConfig {
        load_from(dir.path(), &env_of(vars), || "fallback-host".to_string())
    }

    const COMPLETE: &str = r#"{
        "server_url": "wss://relay.example/ws",
        "oidc_issuer": "https://idp.example",
        "oidc_client_id": "public-id"
    }"#;

    #[test]
    fn no_file_means_unconfigured_not_broken() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = load_with(&dir, &[]);
        assert!(config.server.is_none());
        assert!(config.problem.is_none(), "absence is not a fault");
        assert_eq!(config.device_name, "fallback-host");
    }

    #[test]
    fn a_complete_file_configures_the_core() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, COMPLETE);
        let config = load_with(&dir, &[]);
        let server = config.server.expect("server");
        assert_eq!(server.url, "wss://relay.example/ws");
        assert_eq!(server.oidc_issuer, "https://idp.example");
        assert_eq!(server.oidc_client_id, "public-id");
        assert!(config.problem.is_none());
    }

    #[test]
    fn the_oidc_client_secret_is_optional_and_configurable() {
        // Absent (conformant PKCE IdP): None, and that is NOT a fault.
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, COMPLETE);
        let server = load_with(&dir, &[]).server.expect("server");
        assert_eq!(server.oidc_client_secret, None);

        // Present in the file (Google's case): copied as-is.
        write(
            &dir,
            r#"{
                "server_url": "wss://relay.example/ws",
                "oidc_issuer": "https://idp.example",
                "oidc_client_id": "public-id",
                "oidc_client_secret": "GOCSPX-xyz"
            }"#,
        );
        let server = load_with(&dir, &[]).server.expect("server");
        assert_eq!(server.oidc_client_secret.as_deref(), Some("GOCSPX-xyz"));

        // And the environment overrides it like the rest.
        let server = load_with(&dir, &[("UNIVERSALLINK_OIDC_CLIENT_SECRET", "from-env")])
            .server
            .expect("server");
        assert_eq!(server.oidc_client_secret.as_deref(), Some("from-env"));
    }

    #[test]
    fn the_environment_completes_a_partial_file() {
        // The env > file precedence only holds if completeness is checked
        // AFTER the merge. A partial file validates a deployment where the
        // client_id comes from the environment.
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir,
            r#"{ "server_url": "wss://relay.example/ws", "oidc_issuer": "https://idp.example" }"#,
        );
        let config = load_with(&dir, &[("UNIVERSALLINK_OIDC_CLIENT_ID", "from-env")]);
        assert_eq!(config.server.expect("server").oidc_client_id, "from-env");
        assert!(config.problem.is_none());
    }

    #[test]
    fn the_environment_overrides_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, COMPLETE);
        let config = load_with(&dir, &[("UNIVERSALLINK_SERVER_URL", "ws://127.0.0.1:9/ws")]);
        assert_eq!(config.server.expect("server").url, "ws://127.0.0.1:9/ws");
    }

    #[test]
    fn an_empty_variable_does_not_erase_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, COMPLETE);
        let config = load_with(&dir, &[("UNIVERSALLINK_SERVER_URL", "  ")]);
        assert_eq!(
            config.server.expect("server").url,
            "wss://relay.example/ws",
            "`export VAR=` must not erase a value from the file"
        );
    }

    #[test]
    fn a_half_filled_configuration_is_a_problem_not_a_silent_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "server_url": "wss://relay.example/ws" }"#);
        let config = load_with(&dir, &[]);
        assert!(config.server.is_none());
        let problem = config.problem.expect("a half-setting must be visible");
        assert!(problem.contains("incomplete"), "{problem}");
    }

    #[test]
    fn a_typo_in_a_scheme_is_caught_at_startup() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir,
            r#"{
                "server_url": "https://relay.example/ws",
                "oidc_issuer": "https://idp.example",
                "oidc_client_id": "x"
            }"#,
        );
        let problem = load_with(&dir, &[]).problem.expect("scheme rejected");
        assert!(problem.contains("ws://"), "{problem}");

        write(
            &dir,
            r#"{
                "server_url": "wss://relay.example/ws",
                "oidc_issuer": "idp.example",
                "oidc_client_id": "x"
            }"#,
        );
        let problem = load_with(&dir, &[]).problem.expect("scheme rejected");
        assert!(problem.contains("oidc_issuer"), "{problem}");
    }

    #[test]
    fn a_broken_file_starts_the_core_anyway_and_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, "{ this is not JSON");
        let config = load_with(&dir, &[]);
        assert!(config.server.is_none());
        assert!(config.problem.expect("fault reported").contains("JSON"));
        // And above all: we still have a device name, so something to start with.
        assert_eq!(config.device_name, "fallback-host");
    }

    #[test]
    fn a_wrongly_typed_field_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "server_url": 42 }"#);
        let problem = load_with(&dir, &[]).problem.expect("type rejected");
        assert!(problem.contains("server_url"), "{problem}");
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        // Backward compatibility: a `config.json` written for a newer version
        // must not stop this one from starting.
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir,
            r#"{
                "server_url": "wss://relay.example/ws",
                "oidc_issuer": "https://idp.example",
                "oidc_client_id": "x",
                "future_setting": true
            }"#,
        );
        let config = load_with(&dir, &[]);
        assert!(config.server.is_some());
        assert!(config.problem.is_none());
    }

    #[test]
    fn a_self_hosted_relay_can_be_configured() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "relay_url": "https://iroh-relay.example" }"#);
        let config = load_with(&dir, &[]);
        assert_eq!(
            config.relay_url.expect("relay").to_string(),
            "https://iroh-relay.example/"
        );
        assert!(config.problem.is_none());
        // And the environment overrides, as everywhere.
        let config = load_with(
            &dir,
            &[("UNIVERSALLINK_RELAY_URL", "https://other.example")],
        );
        assert_eq!(
            config.relay_url.expect("relay").to_string(),
            "https://other.example/"
        );
    }

    #[test]
    fn a_broken_relay_url_is_caught_at_startup() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "relay_url": "not a url" }"#);
        let config = load_with(&dir, &[]);
        assert!(config.relay_url.is_none());
        let problem = config.problem.expect("typo reported");
        assert!(problem.contains("relay_url"), "{problem}");
    }

    #[test]
    fn the_receive_dir_falls_back_to_the_config_dir() {
        // Silent environment (no HOME): last resort, always available — the
        // Core must be able to receive even with nothing configured.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = load_with(&dir, &[]);
        assert_eq!(config.receive_dir, dir.path().join("received"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_receive_dir_defaults_to_downloads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = load_from(dir.path(), &env_of(&[("HOME", "/home/u")]), || "h".into());
        assert_eq!(
            config.receive_dir,
            PathBuf::from("/home/u/Downloads/UniversalLink")
        );
        // XDG_DOWNLOAD_DIR wins over ~/Downloads.
        let config = load_from(
            dir.path(),
            &env_of(&[("XDG_DOWNLOAD_DIR", "/data/dl"), ("HOME", "/home/u")]),
            || "h".into(),
        );
        assert_eq!(config.receive_dir, PathBuf::from("/data/dl/UniversalLink"));
    }

    #[test]
    fn the_receive_dir_can_be_configured_and_overridden() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "receive_dir": "/srv/received" }"#);
        assert_eq!(
            load_with(&dir, &[]).receive_dir,
            PathBuf::from("/srv/received")
        );
        // The environment overrides, and an empty variable does not erase.
        let config = load_with(&dir, &[("UNIVERSALLINK_RECEIVE_DIR", "/other/received")]);
        assert_eq!(config.receive_dir, PathBuf::from("/other/received"));
        let config = load_with(&dir, &[("UNIVERSALLINK_RECEIVE_DIR", "  ")]);
        assert_eq!(config.receive_dir, PathBuf::from("/srv/received"));
    }

    #[test]
    fn the_device_name_can_be_chosen() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "device_name": "Living room laptop" }"#);
        assert_eq!(load_with(&dir, &[]).device_name, "Living room laptop");
        assert_eq!(
            load_with(&dir, &[("UNIVERSALLINK_DEVICE_NAME", "Other")]).device_name,
            "Other"
        );
    }
}
