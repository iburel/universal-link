// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! What the daemon needs to know to talk to the deployment: the server URL and
//! the IdP that authenticates it. Nothing secret (the OIDC client is public).
//!
//! Source: `config.json` in the config directory, written by the GUI's setup
//! screen — the daemon only ever READS it, never rewrites it. The
//! `ONEDEVICE_*` environment variables override it, for development.
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

use onedevice_core::ServerConfig;

use crate::dataplane::RelayChoice;

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
    relay: Option<String>,
    /// The pre-#104 key, detected only to be REFUSED with its cure: whoever
    /// set it chose a relay explicitly, and an unknown-field shrug would
    /// silently downgrade that choice to the off default.
    legacy_relay_url: bool,
    receive_dir: Option<String>,
    /// JSON boolean (the only one — the string loop in `read_file` does not
    /// apply to it). `None`: not set, defaults to on.
    lan_discovery: Option<bool>,
}

pub struct DaemonConfig {
    /// `None`: Core not configured. It starts anyway.
    pub server: Option<ServerConfig>,
    pub device_name: String,
    /// The relay, a three-way choice (#104): `"off"` (the default when
    /// nothing is set: no relay, no housekeeping connection), `"n0"` (the n0
    /// public relays, opted into explicitly), or a relay URL (self-hosted, or
    /// a deployment's own).
    pub relay: RelayChoice,
    /// Where received files land. Always set — the Core receives even without
    /// `config.json`: the configured directory, otherwise the user's
    /// downloads, otherwise (silent environment) the config directory.
    pub receive_dir: PathBuf,
    /// mDNS on the local network: announce this device and resolve its
    /// siblings without the relay. ON by default — it only ever discloses the
    /// `node_id` (a public key) and addresses, and trust stays with the
    /// attestations — with `"lan_discovery": false` for networks where even
    /// that is too chatty.
    pub lan_discovery: bool,
    /// The human reason a part of the configuration was not honored, cure
    /// included. Two severities behind the one field: an unreadable or
    /// half-filled file leaves `server` at `None` (the Core starts
    /// unconfigured), while a faulty single setting is simply not applied and
    /// the rest of the config runs, `server` included. Either way the daemon
    /// starts and hands the reason to the Core (`Config::config_problem`), so
    /// the interface can show it: a faulty `config.json` must not deprive the
    /// user of their screen, nor of the sentence naming the fault.
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
    over("ONEDEVICE_DEVICE_NAME", &mut fields.device_name);
    over("ONEDEVICE_RECEIVE_DIR", &mut fields.receive_dir);
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
            // A file we cannot read might have opted in: like the LAN toggle,
            // a broken config falls back to the quiet default, never onto a
            // relay nobody provably chose.
            relay: RelayChoice::Off,
            receive_dir,
            // A file we cannot read might have said `false`: a broken config
            // must not put the machine back on the air, so the toggle fails
            // OFF — unlike an ABSENT file, where on is the honest default.
            lan_discovery: false,
            problem,
        };
    }

    over("ONEDEVICE_SERVER_URL", &mut fields.server_url);
    over("ONEDEVICE_OIDC_ISSUER", &mut fields.oidc_issuer);
    over("ONEDEVICE_OIDC_CLIENT_ID", &mut fields.oidc_client_id);
    over(
        "ONEDEVICE_OIDC_CLIENT_SECRET",
        &mut fields.oidc_client_secret,
    );
    over("ONEDEVICE_RELAY", &mut fields.relay);

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
    // otherwise give a silent data plane, with no explanation. The keywords
    // are matched case-insensitively (they are words, not URLs); anything
    // else must parse as a relay URL. A fault falls back to off, the quiet
    // default (which a server's announcement may fill, #105): never onto n0
    // or a mangled URL nobody provably chose.
    let relay = match fields.relay.as_deref().map(str::trim) {
        None | Some("") => RelayChoice::Off,
        Some(text) if text.eq_ignore_ascii_case("off") => RelayChoice::Off,
        Some(text) if text.eq_ignore_ascii_case("n0") => RelayChoice::N0,
        Some(text) => match text.parse() {
            Ok(url) => RelayChoice::Url(url),
            Err(e) => {
                problem.get_or_insert(format!("relay must be \"off\", \"n0\" or a relay URL: {e}"));
                RelayChoice::Off
            }
        },
    };
    // The pre-#104 spelling named an explicit choice; refusing it with its
    // cure beats silently downgrading that choice to the off default.
    if fields.legacy_relay_url || env("ONEDEVICE_RELAY_URL").is_some_and(|v| !v.trim().is_empty()) {
        problem.get_or_insert(
            "relay_url was replaced by relay (#104): in config.json, set relay to \
             your relay's URL, to \"n0\" for the public relays, or remove it (the \
             default is off)"
                .to_string(),
        );
    }
    // The env override, like everywhere — spelled out because it is a boolean,
    // not a string the `over` helper can move. Garbage neither turns the radio
    // on nor off: the file's intent is clear, the variable's is not — reported,
    // then ignored.
    let lan_discovery = match env("ONEDEVICE_LAN_DISCOVERY")
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
        .as_deref()
    {
        None => fields.lan_discovery.unwrap_or(true),
        Some("true") | Some("1") => true,
        Some("false") | Some("0") => false,
        Some(other) => {
            problem.get_or_insert(format!(
                "ONEDEVICE_LAN_DISCOVERY must be true or false, not {other:?}"
            ));
            fields.lan_discovery.unwrap_or(true)
        }
    };
    DaemonConfig {
        server,
        device_name,
        relay,
        receive_dir,
        lan_discovery,
        problem,
    }
}

/// Where to drop received files. Priority: configured value (`config.json` or
/// `ONEDEVICE_RECEIVE_DIR`) > `<Downloads>/1Device` >
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
        return downloads.join("1Device");
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

#[cfg(target_os = "android")]
fn download_dir(_env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    // Android: the app owns its storage; received files land under the Core's
    // own data dir (`<config dir>/received`). No system Downloads directory is
    // resolved from the environment.
    None
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
        ("relay", &mut fields.relay),
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
    // The pre-#104 key is not one of the unknown fields tolerated below: it
    // named an explicit choice, so `load_from` surfaces it with its cure.
    fields.legacy_relay_url = matches!(object.get("relay_url"), Some(v) if !v.is_null());
    // The only boolean field, read apart from the string loop.
    match object.get("lan_discovery") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Bool(b)) => fields.lan_discovery = Some(*b),
        Some(_) => {
            return Err(format!(
                "lan_discovery must be a boolean in {}",
                path.display()
            ));
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
        let server = load_with(&dir, &[("ONEDEVICE_OIDC_CLIENT_SECRET", "from-env")])
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
        let config = load_with(&dir, &[("ONEDEVICE_OIDC_CLIENT_ID", "from-env")]);
        assert_eq!(config.server.expect("server").oidc_client_id, "from-env");
        assert!(config.problem.is_none());
    }

    #[test]
    fn the_environment_overrides_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, COMPLETE);
        let config = load_with(&dir, &[("ONEDEVICE_SERVER_URL", "ws://127.0.0.1:9/ws")]);
        assert_eq!(config.server.expect("server").url, "ws://127.0.0.1:9/ws");
    }

    #[test]
    fn an_empty_variable_does_not_erase_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, COMPLETE);
        let config = load_with(&dir, &[("ONEDEVICE_SERVER_URL", "  ")]);
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
    fn the_relay_defaults_off() {
        // Nothing set: off, and not a fault (#104: a fresh install contacts
        // no relay nobody chose). The explicit spelling reads the same.
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(load_with(&dir, &[]).relay, RelayChoice::Off);
        write(&dir, COMPLETE);
        assert_eq!(load_with(&dir, &[]).relay, RelayChoice::Off);
        write(&dir, r#"{ "relay": "off" }"#);
        let config = load_with(&dir, &[]);
        assert_eq!(config.relay, RelayChoice::Off);
        assert!(config.problem.is_none());
        // A file we cannot read might have opted in: the fallback is the
        // quiet default, never a relay nobody provably chose.
        write(&dir, "{ this is not JSON");
        assert_eq!(load_with(&dir, &[]).relay, RelayChoice::Off);
    }

    #[test]
    fn n0_and_a_relay_url_are_explicit_choices() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "relay": "n0" }"#);
        assert_eq!(load_with(&dir, &[]).relay, RelayChoice::N0);
        // The keywords are words, not URLs: case does not matter.
        write(&dir, r#"{ "relay": "N0" }"#);
        assert_eq!(load_with(&dir, &[]).relay, RelayChoice::N0);
        write(&dir, r#"{ "relay": "https://iroh-relay.example" }"#);
        let config = load_with(&dir, &[]);
        let url = "https://iroh-relay.example".parse().expect("relay url");
        assert_eq!(config.relay, RelayChoice::Url(url));
        assert!(config.problem.is_none());
        // And the environment overrides, as everywhere, in both directions.
        let config = load_with(&dir, &[("ONEDEVICE_RELAY", "off")]);
        assert_eq!(config.relay, RelayChoice::Off);
        let config = load_with(&dir, &[("ONEDEVICE_RELAY", "https://other.example")]);
        let url = "https://other.example".parse().expect("relay url");
        assert_eq!(config.relay, RelayChoice::Url(url));
    }

    #[test]
    fn a_broken_relay_value_is_caught_at_startup() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "relay": "not a url" }"#);
        let config = load_with(&dir, &[]);
        assert_eq!(config.relay, RelayChoice::Off);
        let problem = config.problem.expect("typo reported");
        assert!(problem.contains("relay"), "{problem}");
    }

    #[test]
    fn the_old_relay_url_spelling_is_refused_with_its_cure() {
        // Whoever set relay_url chose a relay explicitly: the unknown-field
        // shrug would silently downgrade that choice to the off default.
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "relay_url": "https://iroh-relay.example" }"#);
        let config = load_with(&dir, &[]);
        assert_eq!(config.relay, RelayChoice::Off);
        let problem = config.problem.expect("legacy key surfaced");
        assert!(problem.contains("relay_url"), "{problem}");

        // On a fleet device the legacy key sits NEXT TO a working server
        // config: the problem must not cost the device its server. The relay
        // alone falls back, the server keeps running, and the reason travels
        // with it (`Config::config_problem`) so the interface can show the
        // cure. Regression pin: found live on a real client, where the boot
        // log claimed "unconfigured" while the Core ran configured and the
        // screen showed nothing.
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir,
            r#"{
                "server_url": "wss://host/ws",
                "oidc_issuer": "https://idp.example",
                "oidc_client_id": "public-id",
                "relay_url": "https://iroh-relay.example"
            }"#,
        );
        let config = load_with(&dir, &[]);
        assert!(
            config.server.is_some(),
            "a faulty relay spelling must not unconfigure the server"
        );
        assert_eq!(config.relay, RelayChoice::Off);
        let problem = config.problem.expect("legacy key surfaced");
        assert!(problem.contains("relay_url"), "{problem}");

        // The old environment variable gets the same answer.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = load_with(
            &dir,
            &[("ONEDEVICE_RELAY_URL", "https://iroh-relay.example")],
        );
        let problem = config.problem.expect("legacy env surfaced");
        assert!(problem.contains("relay_url"), "{problem}");
        // An empty legacy variable overrides nothing, like everywhere.
        let config = load_with(&dir, &[("ONEDEVICE_RELAY_URL", "  ")]);
        assert!(config.problem.is_none());
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
            PathBuf::from("/home/u/Downloads/1Device")
        );
        // XDG_DOWNLOAD_DIR wins over ~/Downloads.
        let config = load_from(
            dir.path(),
            &env_of(&[("XDG_DOWNLOAD_DIR", "/data/dl"), ("HOME", "/home/u")]),
            || "h".into(),
        );
        assert_eq!(config.receive_dir, PathBuf::from("/data/dl/1Device"));
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
        let config = load_with(&dir, &[("ONEDEVICE_RECEIVE_DIR", "/other/received")]);
        assert_eq!(config.receive_dir, PathBuf::from("/other/received"));
        let config = load_with(&dir, &[("ONEDEVICE_RECEIVE_DIR", "  ")]);
        assert_eq!(config.receive_dir, PathBuf::from("/srv/received"));
    }

    #[test]
    fn lan_discovery_defaults_on_and_the_file_turns_it_off() {
        // Absent (no file, or a file without the field): on — the product
        // default, a fresh install finds its siblings.
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_with(&dir, &[]).lan_discovery);
        write(&dir, COMPLETE);
        assert!(load_with(&dir, &[]).lan_discovery);

        // The file turns it off, and that is not a fault.
        write(&dir, r#"{ "lan_discovery": false }"#);
        let config = load_with(&dir, &[]);
        assert!(!config.lan_discovery);
        assert!(config.problem.is_none());

        // A non-boolean is a type fault, like everywhere.
        write(&dir, r#"{ "lan_discovery": "yes" }"#);
        let problem = load_with(&dir, &[]).problem.expect("type rejected");
        assert!(problem.contains("lan_discovery"), "{problem}");
    }

    #[test]
    fn lan_discovery_fails_off_on_a_broken_file() {
        // The unreadable file might have said `false`: a typo in config.json
        // must not put the machine back on the air.
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, "{ this is not JSON");
        let config = load_with(&dir, &[]);
        assert!(!config.lan_discovery);
        assert!(config.problem.is_some());
    }

    #[test]
    fn lan_discovery_env_overrides_and_garbage_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "lan_discovery": false }"#);

        // The environment overrides the file, in both directions and both
        // spellings.
        assert!(load_with(&dir, &[("ONEDEVICE_LAN_DISCOVERY", "true")]).lan_discovery);
        assert!(load_with(&dir, &[("ONEDEVICE_LAN_DISCOVERY", "1")]).lan_discovery);
        std::fs::remove_file(dir.path().join("config.json")).expect("remove config");
        assert!(!load_with(&dir, &[("ONEDEVICE_LAN_DISCOVERY", "false")]).lan_discovery);
        assert!(!load_with(&dir, &[("ONEDEVICE_LAN_DISCOVERY", "0")]).lan_discovery);

        // An empty variable does not erase, like everywhere.
        write(&dir, r#"{ "lan_discovery": false }"#);
        assert!(!load_with(&dir, &[("ONEDEVICE_LAN_DISCOVERY", "  ")]).lan_discovery);

        // Garbage: reported, and the file's clear intent is kept.
        let config = load_with(&dir, &[("ONEDEVICE_LAN_DISCOVERY", "maybe")]);
        assert!(!config.lan_discovery, "the file said false");
        let problem = config.problem.expect("garbage reported");
        assert!(problem.contains("ONEDEVICE_LAN_DISCOVERY"), "{problem}");
    }

    #[test]
    fn the_device_name_can_be_chosen() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir, r#"{ "device_name": "Living room laptop" }"#);
        assert_eq!(load_with(&dir, &[]).device_name, "Living room laptop");
        assert_eq!(
            load_with(&dir, &[("ONEDEVICE_DEVICE_NAME", "Other")]).device_name,
            "Other"
        );
    }
}
